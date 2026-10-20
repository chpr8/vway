//! vway control protocol v1.
//!
//! Newline-delimited JSON over a session's `ctl.sock`. Every request is an object with an `op`
//! field and an optional `id` that is echoed back; responses are answered in request order per
//! connection. See `docs/design/0012-control-protocol.md` for semantics.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol revision. Independent of the crate version; bumped only on breaking changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest request line the server accepts.
pub const MAX_REQUEST_BYTES: usize = 1 << 20;

// ---------------------------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    Hello {
        protocol: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },
    Status,
    Launch {
        argv: Vec<String>,
        /// Base environment (normally the launching client's). Defaults to the supervisor's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_env: Option<BTreeMap<String, String>>,
        /// Explicit overrides applied after the session policy; `null` unsets.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, Option<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    CloseApp {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grace_ms: Option<u64>,
    },
    Wait {
        #[serde(rename = "for")]
        condition: WaitFor,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_seq: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quiet_ms: Option<u64>,
    },
    Screenshot {
        /// Logical `[x, y, width, height]`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<[f64; 4]>,
        /// Write the PNG here (server side) instead of returning it inline.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// Resample the capture to exactly `[width, height]` pixels (default: physical size).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<[u32; 2]>,
    },
    PointerMove {
        x: f64,
        y: f64,
    },
    PointerButton {
        #[serde(default)]
        button: Button,
        #[serde(default)]
        action: ButtonAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
    },
    Scroll {
        #[serde(default)]
        dx: f64,
        #[serde(default)]
        dy: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<f64>,
    },
    Key {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keys: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<u32>,
        #[serde(default)]
        action: KeyAction,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repeat: Option<u32>,
    },
    Type {
        text: String,
    },
    Touch {
        events: Vec<TouchEvent>,
    },
    Resize {
        width: i32,
        height: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scale: Option<f64>,
    },
    Focus {
        window: u32,
    },
    CloseWindow {
        window: u32,
    },
    Shutdown,
}

impl Op {
    pub fn name(&self) -> &'static str {
        match self {
            Op::Hello { .. } => "hello",
            Op::Status => "status",
            Op::Launch { .. } => "launch",
            Op::CloseApp { .. } => "close_app",
            Op::Wait { .. } => "wait",
            Op::Screenshot { .. } => "screenshot",
            Op::PointerMove { .. } => "pointer_move",
            Op::PointerButton { .. } => "pointer_button",
            Op::Scroll { .. } => "scroll",
            Op::Key { .. } => "key",
            Op::Type { .. } => "type",
            Op::Touch { .. } => "touch",
            Op::Resize { .. } => "resize",
            Op::Focus { .. } => "focus",
            Op::CloseWindow { .. } => "close_window",
            Op::Shutdown => "shutdown",
        }
    }

    /// Whether this op injects input (and therefore counts as activity for `wait idle`).
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            Op::PointerMove { .. }
                | Op::PointerButton { .. }
                | Op::Scroll { .. }
                | Op::Key { .. }
                | Op::Type { .. }
                | Op::Touch { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitFor {
    /// A toplevel has committed a buffer (the app has drawn).
    Window,
    /// A new buffer commit after `after_seq`.
    Frame,
    /// No buffer commits and no injected input for `quiet_ms`.
    Idle,
    /// The app is no longer running.
    AppExit,
}

impl WaitFor {
    pub fn default_timeout_ms(self) -> u64 {
        match self {
            WaitFor::Window | WaitFor::AppExit => 10_000,
            WaitFor::Frame | WaitFor::Idle => 5_000,
        }
    }
}

pub const DEFAULT_QUIET_MS: u64 = 300;

/// A pointer button: a name (`left`, `right`, `middle`, `back`, `forward`) or an evdev code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Button {
    Code(u32),
    Name(String),
}

impl Default for Button {
    fn default() -> Self {
        Button::Name("left".into())
    }
}

impl Button {
    /// The evdev code, or `None` for an unknown name.
    pub fn code(&self) -> Option<u32> {
        match self {
            Button::Code(c) => Some(*c),
            Button::Name(n) => match n.to_ascii_lowercase().as_str() {
                "left" => Some(0x110),
                "right" => Some(0x111),
                "middle" => Some(0x112),
                "side" | "back" => Some(0x113),
                "extra" | "forward" => Some(0x114),
                _ => None,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonAction {
    Down,
    Up,
    #[default]
    Click,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    #[default]
    Press,
    Down,
    Up,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TouchEvent {
    pub phase: TouchPhase,
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub y: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

// ---------------------------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol: u32,
    pub server: String,
    pub session: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutputInfo {
    /// Logical size.
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    /// Screenshot size in physical pixels.
    pub physical_width: i32,
    pub physical_height: i32,
    pub refresh_mhz: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u32,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Has committed a buffer (drawn at least once).
    pub ready: bool,
    pub focused: bool,
    pub parent: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppStatus {
    pub pid: u32,
    pub argv: Vec<String>,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session: String,
    pub pid: u32,
    pub protocol: u32,
    pub uptime_ms: u64,
    pub output: OutputInfo,
    pub pointer: Point,
    pub focus: Option<u32>,
    pub windows: Vec<WindowInfo>,
    pub app: Option<AppStatus>,
    pub commit_seq: u64,
    pub ms_since_commit: u64,
    pub ms_since_input: u64,
    /// Wayland clients the compositor disconnected for protocol errors (most recent last).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub client_errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchResult {
    pub pid: u32,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaitResult {
    pub waited_ms: u64,
    pub commit_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<WindowInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenshotResult {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub png_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// Result of input ops: the commit counter at injection time, for `wait {for: frame, after_seq}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputResult {
    pub commit_seq: u64,
}

/// Written by the supervisor to `info.json` and the readiness pipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub pid: u32,
    pub version: String,
    pub protocol: u32,
    pub session_dir: String,
    pub control_socket: String,
    /// The app's `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY`.
    pub runtime_dir: String,
    pub wayland_display: String,
    pub output: OutputInfo,
    /// Compositor renderer: `gles` (GPU dmabuf import) or `pixman` (CPU only).
    #[serde(default)]
    pub renderer: String,
    pub started_at_unix_ms: u64,
}

/// Written by the supervisor to `exit.json` when it shuts down.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExitInfo {
    /// `shutdown_request`, `app_exited`, `signal_term` / `signal_int` / `signal_hup`, or
    /// `error: <message>` when the supervisor failed after taking the lock.
    pub reason: String,
    pub app: Option<AppStatus>,
    pub exited_at_unix_ms: u64,
}

/// One line on the readiness pipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadyMessage {
    Ready(SessionInfo),
    Error(ErrorBody),
}

// ---------------------------------------------------------------------------------------------
// Responses and errors
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    // Server
    BadRequest,
    UnsupportedProtocol,
    NoApp,
    AppRunning,
    AppExited,
    LaunchFailed,
    NoWindow,
    InvalidKey,
    UnmappableText,
    OutOfRange,
    Timeout,
    RenderFailed,
    IoError,
    ShuttingDown,
    Internal,
    // Client side (CLI, action runner, MCP)
    SessionNotFound,
    SessionExists,
    SpawnFailed,
    PathTooLong,
    ConnectFailed,
    ProtocolError,
    ScriptError,
    NotExecuted,
    NotAllowed,
    Usage,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::UnsupportedProtocol => "unsupported_protocol",
            ErrorCode::NoApp => "no_app",
            ErrorCode::AppRunning => "app_running",
            ErrorCode::AppExited => "app_exited",
            ErrorCode::LaunchFailed => "launch_failed",
            ErrorCode::NoWindow => "no_window",
            ErrorCode::InvalidKey => "invalid_key",
            ErrorCode::UnmappableText => "unmappable_text",
            ErrorCode::OutOfRange => "out_of_range",
            ErrorCode::Timeout => "timeout",
            ErrorCode::RenderFailed => "render_failed",
            ErrorCode::IoError => "io_error",
            ErrorCode::ShuttingDown => "shutting_down",
            ErrorCode::Internal => "internal",
            ErrorCode::SessionNotFound => "session_not_found",
            ErrorCode::SessionExists => "session_exists",
            ErrorCode::SpawnFailed => "spawn_failed",
            ErrorCode::PathTooLong => "path_too_long",
            ErrorCode::ConnectFailed => "connect_failed",
            ErrorCode::ProtocolError => "protocol_error",
            ErrorCode::ScriptError => "script_error",
            ErrorCode::NotExecuted => "not_executed",
            ErrorCode::NotAllowed => "not_allowed",
            ErrorCode::Usage => "usage",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error object of a failed response; also the error type used throughout vway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        ErrorBody {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ErrorBody {}

impl From<std::io::Error> for ErrorBody {
    fn from(e: std::io::Error) -> Self {
        ErrorBody::new(ErrorCode::IoError, e.to_string())
    }
}

/// Shorthand for building an error.
pub fn err(code: ErrorCode, message: impl Into<String>) -> ErrorBody {
    ErrorBody::new(code, message)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn from_result(id: Option<Value>, result: Result<Value, ErrorBody>) -> Self {
        match result {
            Ok(v) => Response {
                id,
                ok: true,
                result: Some(v),
                error: None,
            },
            Err(e) => Response {
                id,
                ok: false,
                result: None,
                error: Some(e),
            },
        }
    }

    pub fn into_result(self) -> Result<Value, ErrorBody> {
        if self.ok {
            Ok(self.result.unwrap_or(Value::Object(Default::default())))
        } else {
            Err(self.error.unwrap_or_else(|| {
                err(
                    ErrorCode::ProtocolError,
                    "error response without error body",
                )
            }))
        }
    }

    /// One protocol line, including the trailing newline.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("response serializes");
        s.push('\n');
        s
    }
}

/// Encode a request as one protocol line, including the trailing newline.
pub fn encode_request(id: Option<Value>, op: &Op) -> String {
    let mut v = serde_json::to_value(op).expect("op serializes");
    if let (Some(id), Value::Object(map)) = (id, &mut v) {
        map.insert("id".into(), id);
    }
    let mut s = v.to_string();
    s.push('\n');
    s
}

/// A request that could not be decoded, with its `id` if it could be read.
pub type RequestError = Box<(Option<Value>, ErrorBody)>;

/// Decode a request line. On failure the `id` (if it could be read) is returned with the error so
/// the response can still be correlated.
pub fn decode_request(line: &str) -> Result<(Option<Value>, Op), RequestError> {
    let mut v: Value = serde_json::from_str(line).map_err(|e| {
        Box::new((
            None,
            err(ErrorCode::BadRequest, format!("invalid JSON: {e}")),
        ))
    })?;
    let id = match &mut v {
        Value::Object(map) => map.remove("id"),
        _ => {
            return Err(Box::new((
                None,
                err(ErrorCode::BadRequest, "request must be a JSON object"),
            )));
        }
    };
    match serde_json::from_value::<Op>(v) {
        Ok(op) => Ok((id, op)),
        Err(e) => Err(Box::new((id, err(ErrorCode::BadRequest, e.to_string())))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_roundtrip_keeps_id() {
        let op = Op::Wait {
            condition: WaitFor::Idle,
            timeout_ms: Some(100),
            title: None,
            app_id: None,
            after_seq: None,
            quiet_ms: Some(50),
        };
        let line = encode_request(Some(json!(7)), &op);
        assert!(line.contains(r#""for":"idle""#), "{line}");
        let (id, back) = decode_request(line.trim()).unwrap();
        assert_eq!(id, Some(json!(7)));
        assert_eq!(back, op);
    }

    #[test]
    fn unit_ops_and_defaults() {
        let (_, op) = decode_request(r#"{"op":"status"}"#).unwrap();
        assert_eq!(op, Op::Status);
        let (_, op) = decode_request(r#"{"op":"pointer_button"}"#).unwrap();
        assert_eq!(
            op,
            Op::PointerButton {
                button: Button::Name("left".into()),
                action: ButtonAction::Click,
                count: None
            }
        );
        let (_, op) = decode_request(r#"{"op":"pointer_button","button":274}"#).unwrap();
        assert!(matches!(
            op,
            Op::PointerButton {
                button: Button::Code(274),
                ..
            }
        ));
    }

    #[test]
    fn unknown_fields_are_rejected_with_id() {
        let (id, e) =
            *decode_request(r#"{"id":"a","op":"pointer_move","x":1,"y":2,"z":3}"#).unwrap_err();
        assert_eq!(id, Some(json!("a")));
        assert_eq!(e.code, ErrorCode::BadRequest);
        assert!(decode_request(r#"{"op":"nope"}"#).is_err());
        assert!(decode_request("[1]").is_err());
    }

    #[test]
    fn button_names() {
        assert_eq!(Button::Name("Right".into()).code(), Some(0x111));
        assert_eq!(Button::Name("bogus".into()).code(), None);
    }

    #[test]
    fn response_roundtrip() {
        let r = Response::from_result(
            Some(json!(1)),
            Err(err(ErrorCode::Timeout, "slow").with_details(json!({"x": 1}))),
        );
        let back: Response = serde_json::from_str(r.to_line().trim()).unwrap();
        let e = back.into_result().unwrap_err();
        assert_eq!(e.code, ErrorCode::Timeout);
        assert_eq!(e.details, Some(json!({"x": 1})));
        assert_eq!(
            serde_json::to_value(ErrorCode::AppExited).unwrap(),
            json!("app_exited")
        );
    }
}
