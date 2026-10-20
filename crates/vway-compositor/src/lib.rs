//! vway's compositor and session supervisor.
//!
//! [`serve`] runs one session: it owns the session lock, a headless Smithay compositor, the
//! control socket, the app, and the private bus, and tears all of them down on every exit path
//! (design 0011).

pub mod config;
mod control;
pub mod env;
mod handlers;
mod input;
pub mod keymap;
pub mod process;
mod render;
mod state;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use calloop::signals::{Signal, Signals};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::{Display, ListeningSocket};
use tracing::{error, info, warn};
use vway_proto::{
    ErrorBody, ErrorCode, ExitInfo, PROTOCOL_VERSION, ReadyMessage, SessionInfo, err,
};

pub use config::SessionConfig;
use config::{DbusMode, HomeMode};
use env::{BusEnv, Policy};
use process::{Exit, kill_descendants, now_unix_ms, reap_all, start_bus, wait_for};
use state::{ClientState, FRAME_INTERVAL, Paths, Phase, State, StateInit};

/// Grace period between SIGTERM and SIGKILL for the app at shutdown.
const APP_GRACE: Duration = Duration::from_secs(2);

/// Run the session in `session_dir` until shutdown. Reports readiness (or the startup error) as one
/// JSON line on `ready_fd`. Returns the process exit code.
pub fn serve(session_dir: &Path, ready_fd: Option<RawFd>) -> i32 {
    init_logging();
    let mut ready = ready_fd
        .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } != -1)
        .map(|fd| unsafe { File::from_raw_fd(fd) });
    let mut report = |msg: ReadyMessage| {
        if let Some(mut f) = ready.take() {
            let mut line = serde_json::to_string(&msg).expect("ready message serializes");
            line.push('\n');
            let _ = f.write_all(line.as_bytes());
        }
    };

    // The lock is held (by keeping the file open) until after exit.json is written.
    let lock = match acquire_lock(session_dir) {
        Ok(l) => l,
        Err(e) => {
            error!("{e}");
            report(ReadyMessage::Error(e));
            return 1;
        }
    };

    match run(session_dir, &mut report) {
        Ok(exit) => {
            write_json(&session_dir.join("exit.json"), &exit);
            info!(reason = %exit.reason, "session ended");
            drop(lock);
            0
        }
        Err(e) => {
            error!("{e}");
            report(ReadyMessage::Error(e.clone()));
            write_json(
                &session_dir.join("exit.json"),
                &ExitInfo {
                    reason: format!("error: {e}"),
                    app: None,
                    exited_at_unix_ms: now_unix_ms(),
                },
            );
            drop(lock);
            1
        }
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("VWAY_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,smithay=warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

fn acquire_lock(dir: &Path) -> Result<File, ErrorBody> {
    let path = dir.join("lock");
    let mut f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .mode(0o600)
        .open(&path)
        .map_err(|e| err(ErrorCode::IoError, format!("open {}: {e}", path.display())))?;
    // Retry briefly: `vway list` probes liveness with a momentary shared lock.
    let mut tries = 0;
    while unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        tries += 1;
        if tries < 20 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        return Err(err(
            ErrorCode::SessionExists,
            format!("session {} is already running", dir.display()),
        ));
    }
    let _ = f.set_len(0);
    let _ = writeln!(f, "{}", std::process::id());
    Ok(f)
}

fn write_json<T: serde::Serialize>(path: &Path, v: &T) {
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(v).expect("serializes");
    if std::fs::write(&tmp, body)
        .and_then(|_| std::fs::rename(&tmp, path))
        .is_err()
    {
        warn!("cannot write {}", path.display());
    }
}

fn mkdir_private(p: &Path) -> Result<(), ErrorBody> {
    std::fs::create_dir_all(p)
        .and_then(|_| std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)))
        .map_err(|e| err(ErrorCode::IoError, format!("create {}: {e}", p.display())))
}

fn io_err(what: &str) -> impl Fn(std::io::Error) -> ErrorBody + '_ {
    move |e| err(ErrorCode::IoError, format!("{what}: {e}"))
}

fn run(dir: &Path, report: &mut dyn FnMut(ReadyMessage)) -> Result<ExitInfo, ErrorBody> {
    let cfg: SessionConfig = std::fs::read(dir.join("session.json"))
        .map_err(io_err("read session.json"))
        .and_then(|b| {
            serde_json::from_slice(&b)
                .map_err(|e| err(ErrorCode::BadRequest, format!("session.json: {e}")))
        })?;
    cfg.validate().map_err(|m| err(ErrorCode::BadRequest, m))?;
    info!(id = %cfg.id, w = cfg.width, h = cfg.height, scale = cfg.scale, "starting session");

    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    }

    let mut event_loop: EventLoop<'static, State> =
        EventLoop::try_new().map_err(|e| err(ErrorCode::Internal, format!("event loop: {e}")))?;
    let handle = event_loop.handle();

    // Signals are blocked and delivered through a signalfd; children reset their mask.
    let signals = Signals::new(&[
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGHUP,
        Signal::SIGCHLD,
    ])
    .map_err(|e| err(ErrorCode::Internal, format!("signals: {e}")))?;
    handle
        .insert_source(signals, |ev, _, state: &mut State| match ev.signal() {
            Signal::SIGCHLD => state.on_sigchld(),
            sig => {
                // "signal_term", "signal_int", "signal_hup"
                let name = format!("{sig:?}").trim_start_matches("SIG").to_lowercase();
                state.begin_shutdown(&format!("signal_{name}"))
            }
        })
        .map_err(|e| err(ErrorCode::Internal, format!("signal source: {e}")))?;

    // Directories.
    let xdg = dir.join("xdg");
    mkdir_private(&xdg)?;
    let (home, temp_home) = match &cfg.home {
        HomeMode::Isolated => {
            let h = dir.join("home");
            (Some(h.clone()), Some(h))
        }
        HomeMode::Dir(p) => (Some(p.clone()), None),
        HomeMode::Host => (None, None),
    };
    if let Some(h) = &home {
        for sub in ["", ".config", ".local/share", ".cache", ".local/state"] {
            mkdir_private(&h.join(sub))?;
        }
    }
    let mesa_cache = cfg.state_root.join("cache").join("mesa");
    let _ = std::fs::create_dir_all(&mesa_cache);

    let mut policy = Policy {
        runtime_dir: xdg.clone(),
        wayland_display: "wayland-0".into(),
        home,
        bus: match cfg.dbus {
            DbusMode::Host => BusEnv::Host,
            _ => BusEnv::None,
        },
        gpu: cfg.gpu,
        mesa_cache,
    };

    let mut warnings = Vec::new();
    let mut bus = None;
    if cfg.dbus == DbusMode::Private {
        let mut bus_env = env::process_env();
        env::apply_display(&mut bus_env, &policy);
        let log = File::options()
            .append(true)
            .create(true)
            .open(dir.join("compositor.log"))
            .map_err(io_err("open compositor.log"))?;
        match start_bus(dir, &xdg.join("bus"), &bus_env, &log) {
            Ok(b) => {
                policy.bus = BusEnv::Private(b.address.clone());
                bus = Some(b);
            }
            Err(e) => {
                warn!("{e}");
                warnings.push(format!(
                    "no private D-Bus ({e}); apps run without a session bus"
                ));
            }
        }
    }

    // Wayland display and socket.
    let display: Display<State> =
        Display::new().map_err(|e| err(ErrorCode::Internal, format!("wayland display: {e}")))?;
    let dh = display.handle();
    let wl_socket = ListeningSocket::bind_absolute(xdg.join("wayland-0"))
        .map_err(|e| err(ErrorCode::IoError, format!("bind wayland socket: {e}")))?;
    handle
        .insert_source(
            Generic::new(wl_socket, Interest::READ, Mode::Level),
            |_, socket, state: &mut State| {
                while let Ok(Some(stream)) = socket.accept() {
                    if let Err(e) = state.dh.insert_client(
                        stream,
                        Arc::new(ClientState {
                            compositor_state: Default::default(),
                            errors: state.client_errors.clone(),
                        }),
                    ) {
                        warn!("insert wayland client: {e}");
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| err(ErrorCode::Internal, format!("wayland socket source: {e}")))?;
    handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state: &mut State| {
                // SAFETY: the display is never dropped while the source is registered.
                if let Err(e) = unsafe { display.get_mut().dispatch_clients(state) } {
                    warn!("dispatch clients: {e}");
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| err(ErrorCode::Internal, format!("display source: {e}")))?;

    // Control socket.
    let ctl_sock = dir.join("ctl.sock");
    let _ = std::fs::remove_file(&ctl_sock);
    let listener = UnixListener::bind(&ctl_sock).map_err(io_err("bind ctl.sock"))?;
    let _ = std::fs::set_permissions(&ctl_sock, std::fs::Permissions::from_mode(0o600));

    let mut state = State::new(StateInit {
        cfg,
        paths: Paths {
            session: dir.to_path_buf(),
            ctl_sock: ctl_sock.clone(),
            app_log: dir.join("app.log"),
            temp_home,
        },
        dh,
        handle: handle.clone(),
        signal: event_loop.get_signal(),
        policy,
        bus,
        warnings,
        control: Default::default(),
    })
    .map_err(|m| err(ErrorCode::Internal, m))?;
    state
        .control_listen(listener)
        .map_err(io_err("control socket"))?;

    handle
        .insert_source(
            Timer::from_duration(FRAME_INTERVAL),
            |_, _, state: &mut State| {
                state.send_frames();
                TimeoutAction::ToDuration(FRAME_INTERVAL)
            },
        )
        .map_err(|e| err(ErrorCode::Internal, format!("frame timer: {e}")))?;

    let info = SessionInfo {
        id: state.cfg.id.clone(),
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: PROTOCOL_VERSION,
        session_dir: dir.to_string_lossy().into_owned(),
        control_socket: ctl_sock.to_string_lossy().into_owned(),
        runtime_dir: xdg.to_string_lossy().into_owned(),
        wayland_display: "wayland-0".into(),
        output: state.output_info(),
        renderer: state.renderer.name().into(),
        started_at_unix_ms: now_unix_ms(),
    };
    write_json(&dir.join("info.json"), &info);
    info!(pid = info.pid, "ready");
    report(ReadyMessage::Ready(info));

    let run_result = event_loop.run(None, &mut state, |state| {
        state.control_tick();
        state.shutdown_tick();
        let _ = state.dh.flush_clients();
    });
    if let Err(e) = run_result {
        error!("event loop: {e}");
        state
            .shutdown_reason
            .get_or_insert_with(|| format!("error: {e}"));
    }

    let exit = state.teardown();
    drop(event_loop); // removes the Wayland socket and its lock file
    drop(state);
    let _ = std::fs::remove_dir_all(&xdg);
    Ok(exit)
}

impl State {
    fn absorb(&mut self, reaped: &[(i32, Exit)]) {
        for (pid, exit) in reaped {
            if let Some(app) = self.app.as_mut().filter(|a| a.pid == *pid)
                && app.exit.is_none()
            {
                info!(pid, ?exit, "app exited");
                app.exit = Some(*exit);
            }
            if self.bus.as_ref().is_some_and(|b| b.pid == *pid) {
                if self.phase == Phase::Running {
                    warn!(?exit, "private bus exited");
                }
                self.bus = None;
            }
        }
    }

    fn on_sigchld(&mut self) {
        let reaped = reap_all();
        let app_was_running = self.app.as_ref().is_some_and(|a| a.running());
        self.absorb(&reaped);
        let app_running = self.app.as_ref().is_some_and(|a| a.running());
        if app_was_running && !app_running && self.cfg.exit_with_app {
            self.begin_shutdown("app_exited");
        }
    }

    /// Start shutting down: stop accepting, fail pending ops, SIGTERM the app, keep serving
    /// Wayland until it exits or the grace period ends.
    pub fn begin_shutdown(&mut self, reason: &str) {
        if self.phase != Phase::Running {
            return;
        }
        info!(reason, "shutting down");
        self.shutdown_reason = Some(reason.to_string());
        self.control_stop_listening();
        self.control_fail_pending(err(ErrorCode::ShuttingDown, "the session is shutting down"));
        if let Some(app) = self.app.as_ref().filter(|a| a.running()) {
            app.signal_group(libc::SIGTERM);
        }
        self.phase = Phase::Terminating(Instant::now() + APP_GRACE);
    }

    fn shutdown_tick(&mut self) {
        if let Phase::Terminating(deadline) = self.phase {
            let app_running = self.app.as_ref().is_some_and(|a| a.running());
            if !app_running || Instant::now() >= deadline {
                self.signal.stop();
            }
        }
    }

    /// Blocking final teardown after the loop: nothing we started survives this.
    fn teardown(&mut self) -> ExitInfo {
        let mut reaped = Vec::new();
        if let Some(app) = self.app.as_ref().filter(|a| a.running()) {
            warn!(pid = app.pid, "app still running; SIGKILL");
            app.signal_group(libc::SIGKILL);
            let pid = app.pid;
            wait_for(pid, Duration::from_secs(1), &mut reaped);
        }
        if let Some(app) = &self.app {
            // Anything left in the app's process group.
            app.signal_group(libc::SIGKILL);
        }
        let keep: Vec<i32> = self.bus.iter().map(|b| b.pid).collect();
        kill_descendants(&keep, Duration::from_secs(1), &mut reaped);
        self.absorb(&reaped);
        if let Some(bus) = self.bus.take() {
            unsafe {
                libc::kill(bus.pid, libc::SIGTERM);
            }
            if !wait_for(bus.pid, Duration::from_secs(1), &mut reaped) {
                unsafe {
                    libc::kill(bus.pid, libc::SIGKILL);
                }
                wait_for(bus.pid, Duration::from_millis(500), &mut reaped);
            }
        }
        self.absorb(&reaped);
        self.control_drain(Duration::from_millis(200));
        let _ = std::fs::remove_file(&self.paths.ctl_sock);
        if let Some(h) = &self.paths.temp_home {
            let _ = std::fs::remove_dir_all(h);
        }
        let _ = std::fs::remove_file(self.paths.session.join("dbus.conf"));
        ExitInfo {
            reason: self
                .shutdown_reason
                .clone()
                .unwrap_or_else(|| "unknown".into()),
            app: self.app.as_ref().map(|a| a.status()),
            exited_at_unix_ms: now_unix_ms(),
        }
    }
}

/// Path of the session's control socket.
pub fn control_socket(session_dir: &Path) -> PathBuf {
    session_dir.join("ctl.sock")
}
