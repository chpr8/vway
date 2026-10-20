//! Child processes: the app, the private bus, reaping, and teardown (design 0011 §5).

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
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

/// A private `dbus-daemon` with no service directories: nothing can be bus-activated.
pub struct Bus {
    pub pid: i32,
    pub address: String,
}

pub fn bus_config(socket: &Path) -> String {
    format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <keep_umask/>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
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

/// Start the bus and wait until it listens. `Err` carries a human-readable reason.
pub fn start_bus(session_dir: &Path, socket: &Path, env: &Env, log: &File) -> Result<Bus, String> {
    let conf: PathBuf = session_dir.join("dbus.conf");
    std::fs::write(&conf, bus_config(socket)).map_err(|e| format!("write dbus.conf: {e}"))?;
    let mut cmd = Command::new("dbus-daemon");
    cmd.arg("--nofork")
        .arg(format!("--config-file={}", conf.display()))
        .arg("--print-address=1")
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(
            log.try_clone()
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null()),
        )
        .process_group(0);
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || prepare_child(Some(libc::SIGTERM), parent));
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run dbus-daemon: {e}"))?;
    let pid = child.id() as i32;
    let stdout = child.stdout.take().expect("piped");
    drop(child);
    // dbus-daemon prints its address once it is listening; EOF means it failed.
    let mut line = String::new();
    let n = BufReader::new(stdout)
        .read_line(&mut line)
        .map_err(|e| format!("reading dbus-daemon address: {e}"))?;
    if n == 0 || line.trim().is_empty() {
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        return Err("dbus-daemon exited during startup (see compositor.log)".into());
    }
    info!(pid, address = line.trim(), "private session bus started");
    Ok(Bus {
        pid,
        address: format!("unix:path={}", socket.display()),
    })
}
