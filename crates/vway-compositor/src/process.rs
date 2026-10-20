//! Child processes: the app, the private bus, reaping, and teardown (design 0011 §5).

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};
use vway_proto::AppStatus;

use crate::env::Env;

/// How a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl Exit {
    fn from_wait_status(status: libc::c_int) -> Option<Exit> {
        if libc::WIFEXITED(status) {
            Some(Exit::Code(libc::WEXITSTATUS(status)))
        } else if libc::WIFSIGNALED(status) {
            Some(Exit::Signal(libc::WTERMSIG(status)))
        } else {
            None
        }
    }
}

/// Prepare a freshly forked child. Call from `pre_exec` only (async-signal-safe calls only).
///
/// Resets the signal mask and dispositions (the supervisor blocks signals for its signal source,
/// and blocked masks survive `exec`), and arms `PR_SET_PDEATHSIG` so the child cannot outlive
/// `parent`.
///
/// # Safety
/// Must only be called between `fork` and `exec`.
pub unsafe fn prepare_child(pdeathsig: Option<libc::c_int>, parent: libc::pid_t) -> io::Result<()> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
        for sig in 1..32 {
            if sig != libc::SIGKILL && sig != libc::SIGSTOP {
                libc::signal(sig, libc::SIG_DFL);
            }
        }
        if let Some(sig) = pdeathsig {
            if libc::prctl(libc::PR_SET_PDEATHSIG, sig as libc::c_ulong, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            // The parent may have died between fork and prctl.
            if libc::getppid() != parent {
                libc::raise(sig);
            }
        }
    }
    Ok(())
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub struct AppProc {
    pub pid: i32,
    pub argv: Vec<String>,
    pub exit: Option<Exit>,
}

impl AppProc {
    pub fn running(&self) -> bool {
        self.exit.is_none()
    }

    pub fn status(&self) -> AppStatus {
        AppStatus {
            pid: self.pid as u32,
            argv: self.argv.clone(),
            running: self.running(),
            exit_code: match self.exit {
                Some(Exit::Code(c)) => Some(c),
                _ => None,
            },
            signal: match self.exit {
                Some(Exit::Signal(s)) => Some(s),
                _ => None,
            },
        }
    }

    /// Signal the app's whole process group (it is the group leader).
    pub fn signal_group(&self, sig: libc::c_int) {
        unsafe {
            libc::kill(-self.pid, sig);
            libc::kill(self.pid, sig);
        }
    }
}

/// Spawn the app in its own process group with stdio appended to `log_path`.
pub fn spawn_app(
    argv: &[String],
    env: &Env,
    cwd: Option<&Path>,
    log_path: &Path,
) -> io::Result<AppProc> {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln!(log, "=== vway launch at {} ms: {:?}", now_unix_ms(), argv)?;
    let log_err = log.try_clone()?;
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || prepare_child(Some(libc::SIGKILL), parent));
    }
    let child = cmd.spawn()?;
    let pid = child.id() as i32;
    // Reaped via waitpid(-1) on SIGCHLD; the std handle is not used again.
    drop(child);
    info!(pid, ?argv, "launched app");
    Ok(AppProc {
        pid,
        argv: argv.to_vec(),
        exit: None,
    })
}

/// Reap every exited child without blocking.
pub fn reap_all() -> Vec<(i32, Exit)> {
    let mut out = Vec::new();
    loop {
        let mut status: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
        if let Some(exit) = Exit::from_wait_status(status) {
            debug!(pid, ?exit, "reaped child");
            out.push((pid, exit));
        }
    }
    out
}

/// Direct children of this process, from `/proc`.
pub fn children_of_self() -> Vec<i32> {
    let me = std::process::id() as i32;
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| {
                    // `pid (comm) state ppid ...`; comm may contain spaces and parentheses.
                    let rest = &s[s.rfind(')')? + 2..];
                    rest.split_whitespace().nth(1)?.parse::<i32>().ok()
                })
                == Some(me)
        })
        .collect()
}

/// SIGKILL every descendant except `keep`, repeatedly, until none remain or `budget` elapses.
/// Descendants re-parent to us as their parents die because we are a subreaper.
pub fn kill_descendants(keep: &[i32], budget: Duration, reaped: &mut Vec<(i32, Exit)>) {
    let deadline = Instant::now() + budget;
    loop {
        reaped.extend(reap_all());
        let victims: Vec<i32> = children_of_self()
            .into_iter()
            .filter(|p| !keep.contains(p))
            .collect();
        if victims.is_empty() || Instant::now() >= deadline {
            if !victims.is_empty() {
                warn!(?victims, "descendants survived SIGKILL");
            }
            return;
        }
        for pid in victims {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Wait (polling) for `pid` to be reaped, up to `budget`.
pub fn wait_for(pid: i32, budget: Duration, reaped: &mut Vec<(i32, Exit)>) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        reaped.extend(reap_all());
        if reaped.iter().any(|(p, _)| *p == pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A private session bus (`dbus-daemon` or dbus-broker) with no service directories: nothing can
/// be bus-activated.
pub struct Bus {
    pub pid: i32,
    pub address: String,
}

/// The bus configuration. dbus-broker reads the same format but ignores (and warns about)
/// eavesdropping, so it only goes to `dbus-daemon`.
pub fn bus_config(socket: &Path, eavesdrop: bool) -> String {
    let (send, receive) = if eavesdrop {
        (r#" eavesdrop="true""#, "\n    <allow eavesdrop=\"true\"/>")
    } else {
        ("", "")
    };
    format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <keep_umask/>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*"{send}/>{receive}
    <allow own="*"/>
  </policy>
</busconfig>
"#,
        xml_escape(&socket.to_string_lossy())
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Start the bus and wait until it listens: `dbus-daemon` when installed, else dbus-broker (the
/// default on Fedora, Arch and others). `Err` carries a human-readable reason.
pub fn start_bus(session_dir: &Path, socket: &Path, env: &Env, log: &File) -> Result<Bus, String> {
    let conf: PathBuf = session_dir.join("dbus.conf");
    match start_dbus_daemon(&conf, socket, env, log) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        r => return r.map_err(|e| e.to_string()),
    }
    if !on_path(env, "dbus-broker-launch") {
        return Err("neither dbus-daemon nor dbus-broker-launch is installed".into());
    }
    start_dbus_broker(&conf, socket, env, log)
}

fn on_path(env: &Env, prog: &str) -> bool {
    env.get("PATH")
        .is_some_and(|p| std::env::split_paths(p).any(|d| d.join(prog).is_file()))
}

fn bus_command(program: &str, env: &Env, log: &File) -> Command {
    let mut cmd = Command::new(program);
    cmd.env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stderr(
            log.try_clone()
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null()),
        )
        .process_group(0);
    cmd
}

/// `Err` of kind `NotFound` when `dbus-daemon` is not installed; other errors are final.
fn start_dbus_daemon(conf: &Path, socket: &Path, env: &Env, log: &File) -> io::Result<Bus> {
    let fail = |m: String| io::Error::other(m);
    std::fs::write(conf, bus_config(socket, true))
        .map_err(|e| fail(format!("write dbus.conf: {e}")))?;
    let mut cmd = bus_command("dbus-daemon", env, log);
    cmd.arg("--nofork")
        .arg(format!("--config-file={}", conf.display()))
        .arg("--print-address=1")
        .stdout(Stdio::piped());
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || prepare_child(Some(libc::SIGTERM), parent));
    }
    let mut child = match cmd.spawn() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(e),
        r => r.map_err(|e| fail(format!("cannot run dbus-daemon: {e}")))?,
    };
    let pid = child.id() as i32;
    let stdout = child.stdout.take().expect("piped");
    drop(child);
    // dbus-daemon prints its address once it is listening; EOF means it failed.
    let mut line = String::new();
    let n = BufReader::new(stdout)
        .read_line(&mut line)
        .map_err(|e| fail(format!("reading dbus-daemon address: {e}")))?;
    if n == 0 || line.trim().is_empty() {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        return Err(fail(
            "dbus-daemon exited during startup (see compositor.log)".into(),
        ));
    }
    info!(
        pid,
        address = line.trim(),
        "private session bus started (dbus-daemon)"
    );
    Ok(Bus {
        pid,
        address: format!("unix:path={}", socket.display()),
    })
}

/// dbus-broker does not listen itself: `dbus-broker-launch` takes the socket the way systemd
/// passes one, as fd 3 named by `LISTEN_FDS` and `LISTEN_PID`. Only the exec'd process knows its
/// pid, so a shell sets `LISTEN_PID` and execs the launcher.
fn start_dbus_broker(conf: &Path, socket: &Path, env: &Env, log: &File) -> Result<Bus, String> {
    std::fs::write(conf, bus_config(socket, false)).map_err(|e| format!("write dbus.conf: {e}"))?;
    let _ = std::fs::remove_file(socket);
    let listener =
        UnixListener::bind(socket).map_err(|e| format!("bind {}: {e}", socket.display()))?;
    let fd = listener.as_raw_fd();
    let mut cmd = bus_command("sh", env, log);
    cmd.arg("-c")
        .arg(r#"LISTEN_PID=$$ exec dbus-broker-launch --scope user --config-file "$1""#)
        .arg("sh")
        .arg(conf)
        .env("LISTEN_FDS", "1")
        .env_remove("LISTEN_FDNAMES")
        .stdout(Stdio::null());
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || {
            prepare_child(Some(libc::SIGTERM), parent)?;
            // dup2 clears close-on-exec on the copy; an fd already at 3 needs it cleared.
            let r = if fd == 3 {
                libc::fcntl(3, libc::F_SETFD, 0)
            } else {
                libc::dup2(fd, 3)
            };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("cannot run dbus-broker-launch: {e}"))?;
    let pid = child.id() as i32;
    drop(child);
    drop(listener);
    if let Err(e) = handshake(socket, Duration::from_secs(5)) {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        return Err(format!(
            "dbus-broker did not start ({e}; see compositor.log)"
        ));
    }
    info!(pid, socket = %socket.display(), "private session bus started (dbus-broker)");
    Ok(Bus {
        pid,
        address: format!("unix:path={}", socket.display()),
    })
}

/// The socket accepts connections before the broker serves them, so readiness is the first step
/// of a client's authentication being answered.
fn handshake(socket: &Path, budget: Duration) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(budget))?;
    let uid = unsafe { libc::getuid() }.to_string();
    let hex: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
    stream.write_all(format!("\0AUTH EXTERNAL {hex}\r\n").as_bytes())?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    if line.starts_with("OK ") {
        Ok(())
    } else if line.is_empty() {
        Err(io::Error::other("connection closed"))
    } else {
        Err(io::Error::other(format!(
            "unexpected reply {:?}",
            line.trim()
        )))
    }
}
