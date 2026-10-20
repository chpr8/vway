//! Session manager: state root, ids, liveness by lock, spawning the supervisor (design 0011).

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::Serialize;
use vway_compositor::SessionConfig;
use vway_proto::{ErrorBody, ErrorCode, ExitInfo, Op, ReadyMessage, SessionInfo, err};

use crate::client::Client;

/// Longest path we bind below a session dir (`xdg/wayland-0`), for the `sun_path` check.
const LONGEST_SOCKET_SUFFIX: &str = "/xdg/wayland-0";
const SUN_PATH_MAX: usize = 107; // 108 bytes including the NUL
const READY_TIMEOUT: Duration = Duration::from_secs(10);

pub fn state_root() -> Result<PathBuf, ErrorBody> {
    if let Some(dir) = std::env::var_os("VWAY_STATE_DIR").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        ensure_private_dir(&dir)?;
        return Ok(dir);
    }
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(rt).join("vway");
        if ensure_private_dir(&dir).is_ok() {
            return Ok(dir);
        }
    }
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/tmp/vway-{uid}"));
    ensure_private_dir(&dir)?;
    let meta = std::fs::symlink_metadata(&dir)
        .map_err(|e| err(ErrorCode::IoError, format!("{}: {e}", dir.display())))?;
    if meta.file_type().is_symlink() || meta.uid() != uid {
        return Err(err(
            ErrorCode::IoError,
            format!(
                "{} is not a directory owned by you; set VWAY_STATE_DIR",
                dir.display()
            ),
        ));
    }
    Ok(dir)
}

fn ensure_private_dir(dir: &Path) -> Result<(), ErrorBody> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| {
            err(
                ErrorCode::IoError,
                format!("cannot create {}: {e}", dir.display()),
            )
        })?;
    let probe = dir.join(".probe");
    std::fs::write(&probe, b"")
        .and_then(|_| std::fs::remove_file(&probe))
        .map_err(|e| {
            err(
                ErrorCode::IoError,
                format!("{} is not writable: {e}", dir.display()),
            )
        })
}

pub fn sessions_dir(root: &Path) -> PathBuf {
    root.join("sessions")
}

pub fn validate_id(id: &str) -> Result<(), ErrorBody> {
    let ok = !id.is_empty()
        && id.len() <= 32
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(err(
            ErrorCode::Usage,
            format!("invalid session id {id:?} (1-32 chars of A-Z a-z 0-9 - _ .)"),
        ))
    }
}

pub fn random_id(prefix: &str) -> String {
    let mut bytes = [0u8; 3];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .is_err()
    {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
            ^ std::process::id();
        bytes = [(t >> 16) as u8, (t >> 8) as u8, t as u8];
    }
    format!("{prefix}{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
}

/// Alive ⇔ someone holds the session lock.
pub fn is_alive(dir: &Path) -> bool {
    let Ok(f) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(dir.join("lock"))
    else {
        return false;
    };
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if rc == 0 {
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
        false
    } else {
        io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

pub fn read_info(dir: &Path) -> Option<SessionInfo> {
    read_json(&dir.join("info.json"))
}

pub fn read_exit(dir: &Path) -> Option<ExitInfo> {
    read_json(&dir.join("exit.json"))
}

fn lock_pid(dir: &Path) -> Option<i32> {
    std::fs::read_to_string(dir.join("lock"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnMode {
    /// Survives the caller (`vway start`).
    Detached,
    /// Dies with the caller (`vway run`, MCP).
    Owned,
}

pub struct Spawned {
    pub id: String,
    pub dir: PathBuf,
    pub info: SessionInfo,
    pub child: Child,
}

/// Create the session directory and spawn its supervisor; returns once it is ready.
pub fn start(
    root: &Path,
    mut cfg: SessionConfig,
    id: Option<&str>,
    replace: bool,
    mode: SpawnMode,
    id_prefix: &str,
) -> Result<Spawned, ErrorBody> {
    cfg.validate().map_err(|m| err(ErrorCode::Usage, m))?;
    let sessions = sessions_dir(root);
    ensure_private_dir(&sessions)?;

    let (id, dir) = match id {
        Some(id) => {
            validate_id(id)?;
            let dir = sessions.join(id);
            if is_alive(&dir) {
                if !replace {
                    return Err(err(
                        ErrorCode::SessionExists,
                        format!("session {id} is already running (use --replace or another --id)"),
                    ));
                }
                stop(&dir, Duration::from_secs(5))?;
            }
            if dir.exists() {
                std::fs::remove_dir_all(&dir).map_err(|e| {
                    err(
                        ErrorCode::IoError,
                        format!("cannot clear {}: {e}", dir.display()),
                    )
                })?;
            }
            (id.to_string(), dir)
        }
        None => loop {
            let id = random_id(id_prefix);
            let dir = sessions.join(&id);
            if !dir.exists() {
                break (id, dir);
            }
        },
    };

    let longest = dir.as_os_str().len() + LONGEST_SOCKET_SUFFIX.len();
    if longest > SUN_PATH_MAX {
        return Err(err(
            ErrorCode::PathTooLong,
            format!(
                "socket path {}{} is {longest} bytes; Unix sockets allow {SUN_PATH_MAX}. \
                 Set VWAY_STATE_DIR to a shorter directory (e.g. VWAY_STATE_DIR=/tmp/v)",
                dir.display(),
                LONGEST_SOCKET_SUFFIX
            ),
        ));
    }

    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(err(
                ErrorCode::SessionExists,
                format!("session {id} was created concurrently"),
            ));
        }
        Err(e) => {
            return Err(err(
                ErrorCode::IoError,
                format!("create {}: {e}", dir.display()),
            ));
        }
    }
    cfg.id = id.clone();
    cfg.state_root = root.to_path_buf();
    std::fs::write(
        dir.join("session.json"),
        serde_json::to_vec_pretty(&cfg).expect("config serializes"),
    )
    .map_err(|e| err(ErrorCode::IoError, format!("write session.json: {e}")))?;

    let (child, info) = spawn_supervisor(&dir, mode)?;
    Ok(Spawned {
        id,
        dir,
        info,
        child,
    })
}

fn spawn_supervisor(dir: &Path, mode: SpawnMode) -> Result<(Child, SessionInfo), ErrorBody> {
    let spawn_err = |m: String| err(ErrorCode::SpawnFailed, m);
    let exe = std::env::current_exe().map_err(|e| spawn_err(format!("current_exe: {e}")))?;
    let log_path = dir.join("compositor.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| spawn_err(format!("open {}: {e}", log_path.display())))?;
    let log2 = log.try_clone().map_err(|e| spawn_err(e.to_string()))?;

    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(spawn_err(format!("pipe: {}", io::Error::last_os_error())));
    }
    let read_end = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    let wfd = write_end.as_raw_fd();

    let mut cmd = Command::new(exe);
    cmd.arg("__serve")
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2));
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || {
            // The readiness pipe becomes fd 3 (dup2 clears CLOEXEC on the copy).
            if wfd == 3 {
                let flags = libc::fcntl(3, libc::F_GETFD);
                libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            } else if libc::dup2(wfd, 3) < 0 {
                return Err(io::Error::last_os_error());
            }
            match mode {
                SpawnMode::Detached => {
                    libc::setsid();
                    vway_compositor::process::prepare_child(None, parent)
                }
                SpawnMode::Owned => {
                    vway_compositor::process::prepare_child(Some(libc::SIGTERM), parent)
                }
            }
        });
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| spawn_err(format!("spawn supervisor: {e}")))?;
    drop(write_end);

    match read_ready(read_end, READY_TIMEOUT) {
        Ok(ReadyMessage::Ready(info)) => Ok((child, info)),
        Ok(ReadyMessage::Error(e)) => {
            let _ = child.wait();
            Err(e.with_details(serde_json::json!({ "logs": dir })))
        }
        Err(msg) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(spawn_err(format!(
                "supervisor did not become ready: {msg}; log tail:\n{}",
                log_tail(&log_path, 15)
            ))
            .with_details(serde_json::json!({ "logs": dir })))
        }
    }
}

fn read_ready(fd: OwnedFd, timeout: Duration) -> Result<ReadyMessage, String> {
    let deadline = Instant::now() + timeout;
    let mut file = File::from(fd);
    let mut buf = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("no answer within {} s", timeout.as_secs()));
        }
        let mut pfd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.to_string());
        }
        if rc == 0 {
            continue;
        }
        let mut chunk = [0u8; 4096];
        match file.read(&mut chunk) {
            Ok(0) => {
                return Err("supervisor exited before reporting readiness".into());
            }
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    return serde_json::from_slice(&buf[..pos])
                        .map_err(|e| format!("bad readiness message: {e}"));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
}

pub fn log_tail(path: &Path, lines: usize) -> String {
    let s = std::fs::read_to_string(path).unwrap_or_default();
    let v: Vec<&str> = s.lines().collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

/// Resolve the session to use: explicit id, `$VWAY_SESSION`, or the only alive session.
pub fn resolve(root: &Path, id: Option<&str>) -> Result<(String, PathBuf), ErrorBody> {
    let env_id = std::env::var("VWAY_SESSION").ok().filter(|s| !s.is_empty());
    let id = id.map(String::from).or(env_id);
    if let Some(id) = id {
        validate_id(&id)?;
        let dir = sessions_dir(root).join(&id);
        if !is_alive(&dir) {
            return Err(err(
                ErrorCode::SessionNotFound,
                format!("no running session {id} (see `vway list`)"),
            ));
        }
        return Ok((id, dir));
    }
    let alive: Vec<SessionEntry> = list(root, false);
    match alive.as_slice() {
        [one] => Ok((one.id.clone(), PathBuf::from(&one.dir))),
        [] => Err(err(
            ErrorCode::SessionNotFound,
            "no running session (start one with `vway start`)",
        )),
        many => Err(err(
            ErrorCode::SessionNotFound,
            format!(
                "{} sessions are running; pick one with -s/--session or VWAY_SESSION: {}",
                many.len(),
                many.iter()
                    .map(|s| s.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// Ask the supervisor to shut down and wait until it has released the lock (full cleanup).
pub fn stop(dir: &Path, timeout: Duration) -> Result<Option<ExitInfo>, ErrorBody> {
    if !is_alive(dir) {
        return Ok(read_exit(dir));
    }
    let asked = Client::connect(dir)
        .and_then(|mut c| c.call(&Op::Shutdown))
        .is_ok();
    if !asked && let Some(pid) = lock_pid(dir) {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    let deadline = Instant::now() + timeout;
    while is_alive(dir) {
        if Instant::now() >= deadline {
            if let Some(pid) = lock_pid(dir) {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
            let hard = Instant::now() + Duration::from_secs(2);
            while is_alive(dir) && Instant::now() < hard {
                std::thread::sleep(Duration::from_millis(20));
            }
            if is_alive(dir) {
                return Err(err(
                    ErrorCode::Internal,
                    format!("session {} did not stop", dir.display()),
                ));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(read_exit(dir))
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionEntry {
    pub id: String,
    pub alive: bool,
    pub dir: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<SessionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitInfo>,
}

pub fn list(root: &Path, all: bool) -> Vec<SessionEntry> {
    let Ok(rd) = std::fs::read_dir(sessions_dir(root)) else {
        return Vec::new();
    };
    let mut out: Vec<SessionEntry> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let dir = e.path();
            let id = e.file_name().into_string().ok()?;
            let alive = is_alive(&dir);
            if !alive && !all {
                return None;
            }
            Some(SessionEntry {
                id,
                alive,
                dir: dir.to_string_lossy().into_owned(),
                info: read_info(&dir),
                exit: if alive { None } else { read_exit(&dir) },
            })
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Delete the directories of dead sessions.
pub fn gc(root: &Path) -> Vec<String> {
    list(root, true)
        .into_iter()
        .filter(|s| !s.alive)
        .filter(|s| std::fs::remove_dir_all(&s.dir).is_ok())
        .map(|s| s.id)
        .collect()
}

/// An owned session (batch `run`, MCP): stopped on drop; its directory is removed unless kept.
pub struct OwnedSession {
    pub id: String,
    pub dir: PathBuf,
    pub info: SessionInfo,
    child: Option<Child>,
    pub keep_dir: bool,
}

impl OwnedSession {
    pub fn start(root: &Path, cfg: SessionConfig, prefix: &str) -> Result<Self, ErrorBody> {
        let s = start(root, cfg, None, false, SpawnMode::Owned, prefix)?;
        Ok(OwnedSession {
            id: s.id,
            dir: s.dir,
            info: s.info,
            child: Some(s.child),
            keep_dir: false,
        })
    }

    /// Stop the supervisor and wait for it; returns its exit record.
    pub fn stop(&mut self) -> Option<ExitInfo> {
        let mut child = self.child.take()?;
        let _ = Client::connect(&self.dir).and_then(|mut c| c.call(&Op::Shutdown));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let exit = read_exit(&self.dir);
        if !self.keep_dir {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
        exit
    }
}

impl Drop for OwnedSession {
    fn drop(&mut self) {
        self.stop();
    }
}
