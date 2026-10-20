//! The control socket: non-blocking connections, request dispatch, pending operations (0012).

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, Mode, PostAction, RegistrationToken};
use tracing::{debug, info, warn};
use vway_proto::{
    DEFAULT_QUIET_MS, ErrorBody, ErrorCode, HelloResult, LaunchResult, MAX_REQUEST_BYTES, Op,
    PROTOCOL_VERSION, Point, Response, ScreenshotResult, SessionStatus, WaitFor, WaitResult,
    decode_request, err,
};

use crate::config::validate_scale;
use crate::keymap::ReverseKeymap;
use crate::process::spawn_app;
use crate::state::{Phase, State, meta};

pub const CAPABILITIES: &[&str] = &[
    "touch",
    "dmabuf",
    "fractional_scale",
    "viewporter",
    "popup_grabs",
    "primary_selection",
];

#[derive(Default)]
pub struct Control {
    listener: Option<RegistrationToken>,
    conns: HashMap<u64, Conn>,
    next_id: u64,
}

struct Conn {
    stream: UnixStream,
    token: Option<RegistrationToken>,
    rbuf: Vec<u8>,
    inbox: VecDeque<String>,
    out: Vec<u8>,
    out_pos: usize,
    pending: Option<(Option<Value>, Pending)>,
    eof: bool,
    dead: bool,
}

enum Pending {
    Wait {
        cond: WaitFor,
        started: Instant,
        deadline: Instant,
        title: Option<String>,
        app_id: Option<String>,
        after_seq: u64,
        quiet: Duration,
    },
    CloseApp {
        kill_at: Instant,
        give_up: Instant,
        killed: bool,
    },
}

enum Outcome {
    Done(Result<Value, ErrorBody>),
    Pending(Pending),
    /// Reply, then begin shutdown.
    Shutdown,
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value, ErrorBody> {
    serde_json::to_value(v).map_err(|e| err(ErrorCode::Internal, e.to_string()))
}

fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

impl State {
    pub fn control_listen(&mut self, listener: UnixListener) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        let token = self
            .handle
            .insert_source(
                Generic::new(listener, Interest::READ, Mode::Level),
                |_, listener, state: &mut State| {
                    loop {
                        match listener.accept() {
                            Ok((stream, _)) => state.control_accept(stream),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) => {
                                warn!("control accept: {e}");
                                break;
                            }
                        }
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.control.listener = Some(token);
        Ok(())
    }

    /// Stop accepting connections (shutdown); existing connections drain.
    pub fn control_stop_listening(&mut self) {
        if let Some(t) = self.control.listener.take() {
            self.handle.remove(t);
        }
        let _ = std::fs::remove_file(&self.paths.ctl_sock);
    }

    fn control_accept(&mut self, stream: UnixStream) {
        let me = unsafe { libc::getuid() };
        if peer_uid(&stream) != Some(me) {
            warn!("rejected control connection from another user");
            return;
        }
        if let Err(e) = stream.set_nonblocking(true) {
            warn!("control connection: {e}");
            return;
        }
        let Ok(poll_fd) = stream.try_clone() else {
            return;
        };
        let id = self.control.next_id;
        self.control.next_id += 1;
        let token = self.handle.insert_source(
            Generic::new(poll_fd, Interest::BOTH, Mode::Edge),
            move |_, _, state: &mut State| {
                state.control_read(id);
                Ok(PostAction::Continue)
            },
        );
        let token = match token {
            Ok(t) => t,
            Err(e) => {
                warn!("register control connection: {e}");
                return;
            }
        };
        debug!(id, "control connection");
        self.control.conns.insert(
            id,
            Conn {
                stream,
                token: Some(token),
                rbuf: Vec::new(),
                inbox: VecDeque::new(),
                out: Vec::new(),
                out_pos: 0,
                pending: None,
                eof: false,
                dead: false,
            },
        );
    }

    fn control_read(&mut self, id: u64) {
        let Some(c) = self.control.conns.get_mut(&id) else {
            return;
        };
        let mut buf = [0u8; 16384];
        loop {
            match c.stream.read(&mut buf) {
                Ok(0) => {
                    c.eof = true;
                    break;
                }
                Ok(n) => c.rbuf.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    c.dead = true;
                    break;
                }
            }
        }
        while let Some(pos) = c.rbuf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = c.rbuf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1])
                .trim()
                .to_string();
            if !line.is_empty() {
                c.inbox.push_back(line);
            }
        }
        if c.rbuf.len() > MAX_REQUEST_BYTES {
            let resp = Response::from_result(
                None,
                Err(err(ErrorCode::BadRequest, "request line exceeds 1 MiB")),
            );
            c.out.extend_from_slice(resp.to_line().as_bytes());
            c.rbuf.clear();
            c.eof = true;
        }
    }

    /// Run after every event-loop dispatch: progress pending ops, dispatch queued requests, flush.
    pub fn control_tick(&mut self) {
        let ids: Vec<u64> = self.control.conns.keys().copied().collect();
        for id in ids {
            self.control_process(id);
            self.control_flush(id);
        }
    }

    fn control_process(&mut self, id: u64) {
        loop {
            let Some(c) = self.control.conns.get_mut(&id) else {
                return;
            };
            if c.dead {
                return;
            }
            if let Some((rid, pending)) = c.pending.take() {
                match self.check_pending(pending) {
                    Ok(done) => self.reply(id, rid, done),
                    Err(still) => {
                        self.control.conns.get_mut(&id).expect("conn").pending = Some((rid, still));
                        return;
                    }
                }
                continue;
            }
            let Some(line) = c.inbox.pop_front() else {
                return;
            };
            let (rid, op) = match decode_request(&line) {
                Ok(x) => x,
                Err(bad) => {
                    let (rid, e) = *bad;
                    self.reply(id, rid, Err(e));
                    continue;
                }
            };
            debug!(op = op.name(), "request");
            match self.handle_op(op) {
                Outcome::Done(r) => self.reply(id, rid, r),
                Outcome::Pending(p) => {
                    self.control.conns.get_mut(&id).expect("conn").pending = Some((rid, p));
                }
                Outcome::Shutdown => {
                    self.reply(id, rid, Ok(json!({})));
                    self.control_flush(id);
                    self.begin_shutdown("shutdown_request");
                }
            }
        }
    }

    fn reply(&mut self, id: u64, rid: Option<Value>, result: Result<Value, ErrorBody>) {
        if let Some(c) = self.control.conns.get_mut(&id) {
            let line = Response::from_result(rid, result).to_line();
            c.out.extend_from_slice(line.as_bytes());
        }
    }

    fn control_flush(&mut self, id: u64) {
        let Some(c) = self.control.conns.get_mut(&id) else {
            return;
        };
        while c.out_pos < c.out.len() && !c.dead {
            match c.stream.write(&c.out[c.out_pos..]) {
                Ok(0) => c.dead = true,
                Ok(n) => c.out_pos += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => c.dead = true,
            }
        }
        if c.out_pos == c.out.len() {
            c.out.clear();
            c.out_pos = 0;
        }
        let finished = c.eof && c.inbox.is_empty() && c.pending.is_none() && c.out.is_empty();
        if c.dead || finished {
            let c = self.control.conns.remove(&id).expect("conn");
            if let Some(t) = c.token {
                self.handle.remove(t);
            }
            debug!(id, "control connection closed");
        }
    }

    /// Best-effort blocking flush of all connections (used right before exit).
    pub fn control_drain(&mut self, budget: Duration) {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline
            && self
                .control
                .conns
                .values()
                .any(|c| !c.out.is_empty() && !c.dead)
        {
            let ids: Vec<u64> = self.control.conns.keys().copied().collect();
            for id in ids {
                self.control_flush(id);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Fail every pending operation (shutdown).
    pub fn control_fail_pending(&mut self, e: ErrorBody) {
        let ids: Vec<u64> = self.control.conns.keys().copied().collect();
        for id in ids {
            let pending = self
                .control
                .conns
                .get_mut(&id)
                .and_then(|c| c.pending.take());
            if let Some((rid, _)) = pending {
                self.reply(id, rid, Err(e.clone()));
            }
        }
    }

    // --- operations ---------------------------------------------------------------------------

    fn handle_op(&mut self, op: Op) -> Outcome {
        if self.phase != Phase::Running {
            return Outcome::Done(Err(err(
                ErrorCode::ShuttingDown,
                "the session is shutting down",
            )));
        }
        let is_input = op.is_input();
        let r = match op {
            Op::Hello { protocol, client } => {
                if protocol != PROTOCOL_VERSION {
                    Err(err(
                        ErrorCode::UnsupportedProtocol,
                        format!(
                            "client speaks protocol {protocol}, this server speaks {PROTOCOL_VERSION}"
                        ),
                    ))
                } else {
                    if let Some(c) = client {
                        debug!(client = c, "hello");
                    }
                    to_value(HelloResult {
                        protocol: PROTOCOL_VERSION,
                        server: format!("vway {}", env!("CARGO_PKG_VERSION")),
                        session: self.cfg.id.clone(),
                        capabilities: CAPABILITIES.iter().map(|s| s.to_string()).collect(),
                    })
                }
            }
            Op::Status => to_value(self.status()),
            Op::Launch {
                argv,
                base_env,
                env,
                cwd,
            } => self.op_launch(argv, base_env, env, cwd),
            Op::CloseApp { grace_ms } => match &self.app {
                None => Err(err(ErrorCode::NoApp, "no app has been launched")),
                Some(app) if !app.running() => to_value(app.status()),
                Some(app) => {
                    app.signal_group(libc::SIGTERM);
                    let now = Instant::now();
                    let grace = Duration::from_millis(grace_ms.unwrap_or(2000));
                    return Outcome::Pending(Pending::CloseApp {
                        kill_at: now + grace,
                        give_up: now + grace + Duration::from_secs(3),
                        killed: false,
                    });
                }
            },
            Op::Wait {
                condition,
                timeout_ms,
                title,
                app_id,
                after_seq,
                quiet_ms,
            } => {
                let now = Instant::now();
                return Outcome::Pending(Pending::Wait {
                    cond: condition,
                    started: now,
                    deadline: now
                        + Duration::from_millis(
                            timeout_ms.unwrap_or_else(|| condition.default_timeout_ms()),
                        ),
                    title,
                    app_id,
                    after_seq: after_seq.unwrap_or(self.commit_seq),
                    quiet: Duration::from_millis(quiet_ms.unwrap_or(DEFAULT_QUIET_MS)),
                });
            }
            Op::Screenshot { region, path, size } => self.op_screenshot(region, path, size),
            Op::PointerMove { x, y } => {
                self.pointer_move(x, y).map(|p| json!({"x": p.x, "y": p.y}))
            }
            Op::PointerButton {
                button,
                action,
                count,
            } => match button.code() {
                Some(code) => {
                    self.pointer_button(code, action, count.unwrap_or(1));
                    Ok(json!({}))
                }
                None => Err(err(
                    ErrorCode::BadRequest,
                    format!(
                        "unknown button {button:?} (left, right, middle, back, forward, or an evdev code)"
                    ),
                )),
            },
            Op::Scroll { dx, dy, x, y } => {
                let moved = match (x, y) {
                    (Some(x), Some(y)) => self.pointer_move(x, y).map(|_| ()),
                    (None, None) => Ok(()),
                    _ => Err(err(
                        ErrorCode::BadRequest,
                        "scroll needs both x and y, or neither",
                    )),
                };
                moved.and_then(|_| self.scroll(dx, dy)).map(|_| json!({}))
            }
            Op::Key {
                keys,
                code,
                action,
                repeat,
            } => {
                let stroke = match (keys, code) {
                    (Some(k), None) => self.keymap.combo(&k),
                    (None, Some(c)) => Ok(crate::keymap::Stroke {
                        mods: vec![],
                        key: ReverseKeymap::evdev(c),
                    }),
                    _ => Err(err(
                        ErrorCode::BadRequest,
                        "key needs exactly one of keys or code",
                    )),
                };
                stroke.map(|s| {
                    self.key_stroke(&s, action, repeat.unwrap_or(1));
                    json!({})
                })
            }
            Op::Type { text } => self.type_text(&text).map(|n| json!({ "typed": n })),
            Op::Touch { events } => self.touch(&events).map(|_| json!({})),
            Op::Resize {
                width,
                height,
                scale,
            } => {
                let scale = scale.unwrap_or(self.cfg.scale);
                let mut probe = self.cfg.clone();
                probe.width = width;
                probe.height = height;
                probe.scale = scale;
                match probe.validate().and_then(|_| validate_scale(scale)) {
                    Ok(()) => {
                        self.resize_output(width, height, scale);
                        to_value(self.output_info())
                    }
                    Err(m) => Err(err(ErrorCode::OutOfRange, m)),
                }
            }
            Op::Focus { window } => match self.window_by_id(window) {
                Some(w) if meta(&w).mapped.get() => {
                    self.focus_window(Some(&w));
                    Ok(json!({}))
                }
                _ => Err(err(
                    ErrorCode::NoWindow,
                    format!("no mapped window {window}"),
                )),
            },
            Op::CloseWindow { window } => match self.window_by_id(window) {
                Some(w) => {
                    w.toplevel().expect("wayland window").send_close();
                    Ok(json!({}))
                }
                None => Err(err(ErrorCode::NoWindow, format!("no window {window}"))),
            },
            Op::Shutdown => return Outcome::Shutdown,
        };
        // Input results carry the commit counter so clients can wait for the reaction.
        let r = match r {
            Ok(Value::Object(mut m)) if is_input => {
                m.insert("commit_seq".into(), json!(self.commit_seq));
                Ok(Value::Object(m))
            }
            other => other,
        };
        Outcome::Done(r)
    }

    fn op_launch(
        &mut self,
        argv: Vec<String>,
        base_env: Option<std::collections::BTreeMap<String, String>>,
        env: std::collections::BTreeMap<String, Option<String>>,
        cwd: Option<String>,
    ) -> Result<Value, ErrorBody> {
        if argv.first().is_none_or(|a| a.is_empty()) {
            return Err(err(ErrorCode::BadRequest, "argv must not be empty"));
        }
        if let Some(app) = &self.app
            && app.running()
        {
            return Err(err(
                    ErrorCode::AppRunning,
                    format!(
                        "app {} is still running (one app per session; close it or start another session)",
                        app.pid
                    ),
                )
                .with_details(to_value(app.status())?));
        }
        let base = match base_env {
            Some(m) => m,
            None => crate::env::process_env(),
        };
        // Lets `vway` invoked from inside the app find this session; overridable like any variable.
        let session_vars = std::collections::BTreeMap::from([
            ("VWAY_SESSION".to_string(), Some(self.cfg.id.clone())),
            (
                "VWAY_STATE_DIR".to_string(),
                Some(self.cfg.state_root.to_string_lossy().into_owned()),
            ),
        ]);
        let (app_env, mut warnings) =
            crate::env::build(base, &self.policy, &[&session_vars, &self.cfg.env, &env]);
        warnings.extend(self.warnings.iter().cloned());
        let cwd = cwd.map(std::path::PathBuf::from);
        let app = spawn_app(&argv, &app_env, cwd.as_deref(), &self.paths.app_log).map_err(|e| {
            err(
                ErrorCode::LaunchFailed,
                format!("cannot run {:?}: {e}", argv[0]),
            )
        })?;
        let pid = app.pid as u32;
        self.app = Some(app);
        to_value(LaunchResult { pid, warnings })
    }

    fn op_screenshot(
        &mut self,
        region: Option<[f64; 4]>,
        path: Option<String>,
        size: Option<[u32; 2]>,
    ) -> Result<Value, ErrorBody> {
        let cap = self.capture(region, size)?;
        let mut res = ScreenshotResult {
            width: cap.width,
            height: cap.height,
            scale: self.cfg.scale,
            png_base64: None,
            path: None,
        };
        match path {
            Some(p) => {
                std::fs::write(Path::new(&p), &cap.png)
                    .map_err(|e| err(ErrorCode::IoError, format!("cannot write {p}: {e}")))?;
                res.path = Some(p);
            }
            None => {
                res.png_base64 = Some(base64::engine::general_purpose::STANDARD.encode(&cap.png));
            }
        }
        to_value(res)
    }

    pub fn status(&self) -> SessionStatus {
        let now = Instant::now();
        let focus = self.focused_window().map(|w| meta(&w).id);
        SessionStatus {
            session: self.cfg.id.clone(),
            pid: std::process::id(),
            protocol: PROTOCOL_VERSION,
            uptime_ms: self.started.elapsed().as_millis() as u64,
            output: self.output_info(),
            pointer: Point {
                x: self.pointer.x,
                y: self.pointer.y,
            },
            focus,
            windows: self.windows.iter().map(|w| self.window_info(w)).collect(),
            app: self.app.as_ref().map(|a| a.status()),
            commit_seq: self.commit_seq,
            ms_since_commit: (now - self.last_commit).as_millis() as u64,
            ms_since_input: (now - self.last_input).as_millis() as u64,
            client_errors: self.client_errors_since(None),
        }
    }

    /// Messages of client protocol errors, optionally only those after `since`.
    fn client_errors_since(&self, since: Option<Instant>) -> Vec<String> {
        self.client_errors
            .lock()
            .map(|e| {
                e.iter()
                    .filter(|(t, _)| since.is_none_or(|s| *t >= s))
                    .map(|(_, m)| m.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn app_exited(&self) -> bool {
        self.app.as_ref().is_some_and(|a| !a.running())
    }

    /// `Ok(result)` when the pending op is finished, `Err(pending)` to keep waiting.
    fn check_pending(&mut self, p: Pending) -> Result<Result<Value, ErrorBody>, Pending> {
        let now = Instant::now();
        match p {
            Pending::Wait {
                cond,
                started,
                deadline,
                ref title,
                ref app_id,
                after_seq,
                quiet,
            } => {
                let done = |state: &State, window| {
                    to_value(WaitResult {
                        waited_ms: (now - started).as_millis() as u64,
                        commit_seq: state.commit_seq,
                        window,
                    })
                };
                let any_ready = self.windows.iter().any(|w| meta(w).ready.get());
                match cond {
                    WaitFor::Window => {
                        let found = self.windows.iter().find(|w| {
                            let info = self.window_info(w);
                            info.ready
                                && title.as_ref().is_none_or(|t| {
                                    info.title
                                        .as_deref()
                                        .is_some_and(|x| x.contains(t.as_str()))
                                })
                                && app_id.as_ref().is_none_or(|a| {
                                    info.app_id
                                        .as_deref()
                                        .is_some_and(|x| x.contains(a.as_str()))
                                })
                        });
                        if let Some(w) = found {
                            return Ok(done(self, Some(self.window_info(w))));
                        }
                    }
                    WaitFor::Frame => {
                        if self.commit_seq > after_seq {
                            return Ok(done(self, None));
                        }
                    }
                    WaitFor::Idle => {
                        if any_ready
                            && now - self.last_commit >= quiet
                            && now - self.last_input >= quiet
                        {
                            return Ok(done(self, None));
                        }
                    }
                    WaitFor::AppExit => {
                        if self.app.as_ref().is_none_or(|a| !a.running()) {
                            return Ok(done(self, None));
                        }
                    }
                }
                let fail_fast = match cond {
                    WaitFor::Window | WaitFor::Frame => self.app_exited(),
                    WaitFor::Idle => self.app_exited() && !any_ready,
                    WaitFor::AppExit => false,
                };
                // A client killed by a protocol error (e.g. a buffer the compositor could not
                // import) usually never shows a window; say so instead of timing out.
                if matches!(cond, WaitFor::Window | WaitFor::Frame)
                    && !any_ready
                    && let Some(e) = self.client_errors_since(Some(started)).pop()
                {
                    return Ok(Err(err(
                        ErrorCode::ProtocolError,
                        format!("the app's Wayland connection was terminated: {e}"),
                    )
                    .with_details(json!({ "status": self.status() }))));
                }
                if fail_fast {
                    let app = self.app.as_ref().map(|a| a.status());
                    return Ok(Err(err(
                        ErrorCode::AppExited,
                        format!(
                            "the app exited while waiting for {cond:?} (see app.log); status: {}",
                            app.as_ref()
                                .map(|a| match (a.exit_code, a.signal) {
                                    (Some(c), _) => format!("exit code {c}"),
                                    (_, Some(s)) => format!("signal {s}"),
                                    _ => "unknown".into(),
                                })
                                .unwrap_or_default()
                        ),
                    )
                    .with_details(json!({ "status": self.status() }))));
                }
                if now >= deadline {
                    return Ok(Err(err(
                        ErrorCode::Timeout,
                        format!(
                            "timed out after {} ms waiting for {}",
                            (now - started).as_millis(),
                            serde_json::to_value(cond)
                                .ok()
                                .and_then(|v| v.as_str().map(String::from))
                                .unwrap_or_default()
                        ),
                    )
                    .with_details(json!({ "status": self.status() }))));
                }
                Err(p)
            }
            Pending::CloseApp {
                kill_at,
                give_up,
                killed,
            } => {
                let Some(app) = &self.app else {
                    return Ok(Err(err(ErrorCode::NoApp, "no app")));
                };
                if !app.running() {
                    // Leftovers in the app's process group go with it.
                    app.signal_group(libc::SIGKILL);
                    info!(pid = app.pid, "app closed");
                    return Ok(to_value(app.status()));
                }
                if now >= give_up {
                    return Ok(Err(err(
                        ErrorCode::Internal,
                        "the app survived SIGKILL (uninterruptible sleep?)",
                    )));
                }
                if now >= kill_at && !killed {
                    warn!(pid = app.pid, "app ignored SIGTERM; sending SIGKILL");
                    app.signal_group(libc::SIGKILL);
                    return Err(Pending::CloseApp {
                        kill_at,
                        give_up,
                        killed: true,
                    });
                }
                Err(Pending::CloseApp {
                    kill_at,
                    give_up,
                    killed,
                })
            }
        }
    }
}
