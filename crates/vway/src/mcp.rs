//! `vway mcp`: a Model Context Protocol server on stdio (design 0011 §7).
//!
//! Newline-delimited JSON-RPC 2.0, hand-rolled: `initialize`, `ping`, `tools/list`, `tools/call`.
//! Sessions started here are owned: they die with this process and are stopped on stdin EOF.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use vway_proto::{ErrorBody, ErrorCode, Op, WaitFor, err};

use crate::actions::{ImageLimit, Images, Runner, Space, parse_script};
use crate::client::Client;
use crate::opts::SessionOpts;
use crate::session::{self, OwnedSession};

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub struct Server {
    root: PathBuf,
    allow: Vec<String>,
    limit: ImageLimit,
    sessions: HashMap<String, OwnedSession>,
}

/// A tool's output: MCP content blocks plus whether it is an error result.
struct ToolOutput {
    content: Vec<Value>,
    is_error: bool,
}

impl ToolOutput {
    fn json(v: &Value) -> Self {
        ToolOutput {
            content: vec![text_block(v)],
            is_error: false,
        }
    }

    fn error(e: &ErrorBody) -> Self {
        ToolOutput {
            content: vec![text_block(&json!({ "ok": false, "error": e }))],
            is_error: true,
        }
    }
}

fn text_block(v: &Value) -> Value {
    json!({ "type": "text", "text": serde_json::to_string_pretty(v).unwrap_or_default() })
}

fn image_block(b64: &str) -> Value {
    json!({ "type": "image", "data": b64, "mimeType": "image/png" })
}

/// Move inline `png_base64` screenshot data out of `v` into image blocks.
fn extract_images(v: &mut Value, images: &mut Vec<Value>) {
    match v {
        Value::Object(o) => {
            if let Some(Value::String(b64)) = o.remove("png_base64") {
                images.push(image_block(&b64));
                o.insert("image".into(), json!(images.len()));
            }
            for (_, child) in o.iter_mut() {
                extract_images(child, images);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|c| extract_images(c, images)),
        _ => {}
    }
}

impl Server {
    pub fn new(root: PathBuf, allow: Vec<String>, limit: ImageLimit) -> Self {
        Server {
            root,
            allow,
            limit,
            sessions: HashMap::new(),
        }
    }

    /// Serve until stdin closes, then stop every owned session.
    pub fn run(mut self) -> std::io::Result<()> {
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout().lock();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(resp) = self.handle_line(&line) {
                writeln!(stdout, "{resp}")?;
                stdout.flush()?;
            }
        }
        self.shutdown();
        Ok(())
    }

    pub fn shutdown(&mut self) {
        for (_, mut s) in self.sessions.drain() {
            s.stop();
        }
    }

    fn handle_line(&mut self, line: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => return Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Value::Array(batch) = &msg {
            let out: Vec<Value> = batch.iter().filter_map(|m| self.handle(m)).collect();
            return (!out.is_empty()).then(|| Value::Array(out));
        }
        self.handle(&msg)
    }

    fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            // A response to something we never send, or garbage.
            return id.map(|id| rpc_error(id, -32600, "invalid request"));
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        // Notifications (no id) get no response.
        let id = id?;
        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.call_tool(&params),
            _ => Err((-32601, format!("method {method} not found"))),
        };
        Some(match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err((code, m)) => rpc_error(id, code, &m),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = requested
            .filter(|v| PROTOCOL_VERSIONS.contains(v))
            .unwrap_or(PROTOCOL_VERSIONS[0]);
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "vway", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "vway runs GUI apps in headless Wayland desktops. Start a session with \
                `start` (pass `command` to launch the app), then drive it with `computer` \
                (Anthropic computer-use actions plus vway_* extensions) and look at it with \
                `screenshot`. Coordinates are logical pixels. Sessions end when this server exits.",
        })
    }

    fn call_tool(&mut self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "tools/call needs a name".to_string()))?;
        let args = match params.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(o)) => o.clone(),
            Some(_) => return Err((-32602, "arguments must be an object".into())),
        };
        let out = match name {
            "start" => self.tool_start(args),
            "launch" => self.tool_launch(args),
            "computer" => self.tool_computer(args),
            "screenshot" => self.tool_screenshot(args),
            "status" => self.tool_status(args),
            "stop" => self.tool_stop(args),
            "list" => Ok(self.tool_list()),
            _ => return Err((-32602, format!("unknown tool {name}"))),
        };
        let out = out.unwrap_or_else(|e| ToolOutput::error(&e));
        Ok(json!({ "content": out.content, "isError": out.is_error }))
    }

    /// With `--allow`, resolve the program to a canonical path and require it to match an entry:
    /// entries ending in `/` are directories (anything below them), others are exact programs
    /// (bare names are looked up on this server's `PATH`). Returns argv with the resolved program.
    /// A guard rail against accidents, not a sandbox: allowed programs can still run anything.
    fn check_allowed(
        &self,
        argv: &[String],
        env: &BTreeMap<String, Option<String>>,
    ) -> Result<Vec<String>, ErrorBody> {
        let Some(prog) = argv.first().filter(|p| !p.is_empty()) else {
            return Err(err(ErrorCode::BadRequest, "command must not be empty"));
        };
        if self.allow.is_empty() {
            return Ok(argv.to_vec());
        }
        if let Some(k) = env.keys().find(|k| k.starts_with("LD_") || *k == "PATH") {
            return Err(err(
                ErrorCode::NotAllowed,
                format!("overriding {k} is not allowed when the server has an --allow list"),
            ));
        }
        let denied = || {
            err(
                ErrorCode::NotAllowed,
                format!(
                    "{prog:?} is not allowed by this server (allowed: {})",
                    self.allow.join(", ")
                ),
            )
        };
        let resolved = resolve_program(prog).ok_or_else(denied)?;
        let ok = self.allow.iter().any(|entry| {
            if entry.ends_with('/') {
                std::fs::canonicalize(entry).is_ok_and(|d| resolved.starts_with(d))
            } else {
                resolve_program(entry).is_some_and(|p| p == resolved)
            }
        });
        if !ok {
            return Err(denied());
        }
        let mut out = argv.to_vec();
        out[0] = resolved.to_string_lossy().into_owned();
        Ok(out)
    }

    /// Resolve `session` to a directory: owned sessions first, then any live session by id.
    fn session_dir(&self, args: &Map<String, Value>) -> Result<(String, PathBuf), ErrorBody> {
        let id = str_arg(args, "session")?;
        match id {
            Some(id) => {
                if let Some(s) = self.sessions.get(id) {
                    return Ok((s.id.clone(), s.dir.clone()));
                }
                session::resolve(&self.root, Some(id))
            }
            None if self.sessions.len() == 1 => {
                let s = self.sessions.values().next().expect("one session");
                Ok((s.id.clone(), s.dir.clone()))
            }
            None if self.sessions.is_empty() => Err(err(
                ErrorCode::SessionNotFound,
                "no session: call start first",
            )),
            None => Err(err(
                ErrorCode::BadRequest,
                format!(
                    "several sessions are running ({}); pass `session`",
                    self.sessions.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            )),
        }
    }

    fn tool_start(&mut self, mut args: Map<String, Value>) -> Result<ToolOutput, ErrorBody> {
        let command = take_argv(&mut args, "command")?;
        let env = take_env(&mut args, "env")?;
        let screenshot = take_bool(&mut args, "screenshot")?.unwrap_or(true);
        let timeout_ms = take_u64(&mut args, "timeout_ms")?;
        let command = match command {
            Some(argv) => Some(self.check_allowed(&argv, &env)?),
            None => None,
        };
        let opts: SessionOpts = serde_json::from_value(Value::Object(args))
            .map_err(|e| err(ErrorCode::BadRequest, format!("start: {e}")))?;
        let cfg = opts.to_config(&self.root)?;
        let owned = OwnedSession::start(&self.root, cfg, "mcp-")?;
        let id = owned.id.clone();
        let dir = owned.dir.clone();
        let mut out = json!({ "ok": true, "session": id, "info": owned.info });
        self.sessions.insert(id.clone(), owned);
        let mut content = Vec::new();
        if let Some(argv) = command {
            let launched =
                launch_and_wait(&dir, argv, env, None, timeout_ms, screenshot, self.limit);
            merge_launch(&mut out, &mut content, launched);
        }
        content.insert(0, text_block(&out));
        Ok(ToolOutput {
            is_error: out["ok"] == json!(false),
            content,
        })
    }

    fn tool_launch(&mut self, mut args: Map<String, Value>) -> Result<ToolOutput, ErrorBody> {
        let (id, dir) = self.session_dir(&args)?;
        args.remove("session");
        let argv = take_argv(&mut args, "command")?
            .ok_or_else(|| err(ErrorCode::BadRequest, "launch needs `command`"))?;
        let env = take_env(&mut args, "env")?;
        let argv = self.check_allowed(&argv, &env)?;
        let cwd = take_str(&mut args, "cwd")?;
        let screenshot = take_bool(&mut args, "screenshot")?.unwrap_or(true);
        let timeout_ms = take_u64(&mut args, "timeout_ms")?;
        reject_rest("launch", &args)?;
        let mut out = json!({ "ok": true, "session": id });
        let mut content = Vec::new();
        let launched = launch_and_wait(&dir, argv, env, cwd, timeout_ms, screenshot, self.limit);
        merge_launch(&mut out, &mut content, launched);
        content.insert(0, text_block(&out));
        Ok(ToolOutput {
            is_error: out["ok"] == json!(false),
            content,
        })
    }

    fn tool_computer(&mut self, mut args: Map<String, Value>) -> Result<ToolOutput, ErrorBody> {
        let (_, dir) = self.session_dir(&args)?;
        args.remove("session");
        // Either a batch (`actions`) or a single Anthropic-style action (`action` + its fields).
        let actions = match args.remove("actions") {
            Some(v) => {
                if !args.is_empty() {
                    return Err(err(
                        ErrorCode::ScriptError,
                        "pass either `actions` or a single action, not both",
                    ));
                }
                parse_script(&v)?
            }
            None => vec![Value::Object(args)],
        };
        let mut client = Client::connect(&dir)?;
        let report =
            Runner::new(&mut client, Images::Inline, Space::Screenshot(self.limit)).run(&actions);
        let mut v = serde_json::to_value(&report).unwrap_or_default();
        let mut images = Vec::new();
        extract_images(&mut v, &mut images);
        let mut content = vec![text_block(&v)];
        content.extend(images);
        Ok(ToolOutput {
            content,
            is_error: !report.ok,
        })
    }

    fn tool_screenshot(&mut self, mut args: Map<String, Value>) -> Result<ToolOutput, ErrorBody> {
        let (_, dir) = self.session_dir(&args)?;
        args.remove("session");
        let mut action = json!({ "type": "screenshot" });
        for k in ["region", "settle"] {
            if let Some(v) = args.remove(k) {
                action[k] = v;
            }
        }
        reject_rest("screenshot", &args)?;
        let mut client = Client::connect(&dir)?;
        let mut v = Runner::new(&mut client, Images::Inline, Space::Screenshot(self.limit))
            .run_one(&action)?;
        let mut images = Vec::new();
        extract_images(&mut v, &mut images);
        let mut content = vec![text_block(&v)];
        content.extend(images);
        Ok(ToolOutput {
            content,
            is_error: false,
        })
    }

    fn tool_status(&mut self, mut args: Map<String, Value>) -> Result<ToolOutput, ErrorBody> {
        let (_, dir) = self.session_dir(&args)?;
        args.remove("session");
        reject_rest("status", &args)?;
        let status = Client::connect(&dir)?.call(&Op::Status)?;
        Ok(ToolOutput::json(&status))
    }

    fn tool_stop(&mut self, mut args: Map<String, Value>) -> Result<ToolOutput, ErrorBody> {
        let (id, dir) = self.session_dir(&args)?;
        args.remove("session");
        reject_rest("stop", &args)?;
        let exit = match self.sessions.remove(&id) {
            Some(mut s) => s.stop(),
            None => session::stop(&dir, std::time::Duration::from_secs(5))?,
        };
        Ok(ToolOutput::json(
            &json!({ "ok": true, "session": id, "exit": exit }),
        ))
    }

    fn tool_list(&self) -> ToolOutput {
        let list: Vec<Value> = session::list(&self.root, false)
            .into_iter()
            .map(|e| {
                let mut v = serde_json::to_value(&e).unwrap_or_default();
                v["owned"] = json!(self.sessions.contains_key(&e.id));
                v
            })
            .collect();
        ToolOutput::json(&json!({ "ok": true, "sessions": list }))
    }
}

/// Launch, wait for the first ready window, optionally screenshot. Errors carry the app log tail.
fn launch_and_wait(
    dir: &Path,
    argv: Vec<String>,
    env: BTreeMap<String, Option<String>>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
    screenshot: bool,
    limit: ImageLimit,
) -> Result<(Value, Option<String>), ErrorBody> {
    let with_log = |e: ErrorBody| {
        let tail = session::log_tail(&dir.join("app.log"), 30);
        if tail.is_empty() {
            e
        } else {
            let mut d = e.details.clone().unwrap_or(json!({}));
            if d.is_object() {
                d["app_log_tail"] = json!(tail);
            }
            e.with_details(d)
        }
    };
    let mut client = Client::connect(dir)?;
    let launched = client.call(&Op::Launch {
        argv,
        base_env: None,
        env,
        cwd,
    })?;
    let waited = client
        .call(&Op::Wait {
            condition: WaitFor::Window,
            timeout_ms,
            title: None,
            app_id: None,
            after_seq: None,
            quiet_ms: None,
        })
        .map_err(with_log)?;
    let mut out = json!({ "launch": launched, "window": waited.get("window") });
    let mut image = None;
    if screenshot {
        let mut shot = Runner::new(&mut client, Images::Inline, Space::Screenshot(limit))
            .run_one(&json!({ "type": "screenshot" }))?;
        image = shot
            .as_object_mut()
            .and_then(|o| o.remove("png_base64"))
            .and_then(|v| v.as_str().map(String::from));
        out["screenshot"] = shot;
    }
    Ok((out, image))
}

fn merge_launch(
    out: &mut Value,
    content: &mut Vec<Value>,
    launched: Result<(Value, Option<String>), ErrorBody>,
) {
    match launched {
        Ok((v, image)) => {
            for (k, val) in v.as_object().into_iter().flatten() {
                out[k] = val.clone();
            }
            if let Some(b64) = image {
                content.push(image_block(&b64));
            }
        }
        Err(e) => {
            out["ok"] = json!(false);
            out["error"] = serde_json::to_value(&e).unwrap_or_default();
        }
    }
}

/// Canonical path of a program: paths relative to the cwd, bare names via `PATH`.
fn resolve_program(prog: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let is_exe = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let found = if prog.contains('/') {
        Some(PathBuf::from(prog))
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(prog))
            .find(|p| is_exe(p))
    }?;
    std::fs::canonicalize(found).ok().filter(|p| is_exe(p))
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn bad(m: String) -> ErrorBody {
    err(ErrorCode::BadRequest, m)
}

fn str_arg<'a>(args: &'a Map<String, Value>, k: &str) -> Result<Option<&'a str>, ErrorBody> {
    match args.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad(format!("{k} must be a string"))),
    }
}

fn take_str(args: &mut Map<String, Value>, k: &str) -> Result<Option<String>, ErrorBody> {
    let v = str_arg(args, k)?.map(String::from);
    args.remove(k);
    Ok(v)
}

fn take_bool(args: &mut Map<String, Value>, k: &str) -> Result<Option<bool>, ErrorBody> {
    match args.remove(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(b)),
        Some(_) => Err(bad(format!("{k} must be a boolean"))),
    }
}

fn take_u64(args: &mut Map<String, Value>, k: &str) -> Result<Option<u64>, ErrorBody> {
    match args.remove(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| bad(format!("{k} must be a non-negative integer"))),
    }
}

/// `command` as an argv array, or a string split on whitespace for convenience.
fn take_argv(args: &mut Map<String, Value>, k: &str) -> Result<Option<Vec<String>>, ErrorBody> {
    match args.remove(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let argv: Vec<String> = s.split_whitespace().map(String::from).collect();
            if argv.is_empty() {
                return Err(bad(format!("{k} must not be empty")));
            }
            Ok(Some(argv))
        }
        Some(Value::Array(a)) => {
            let argv = a
                .into_iter()
                .map(|v| match v {
                    Value::String(s) => Ok(s),
                    _ => Err(bad(format!("{k} must be an array of strings"))),
                })
                .collect::<Result<Vec<_>, _>>()?;
            if argv.is_empty() {
                return Err(bad(format!("{k} must not be empty")));
            }
            Ok(Some(argv))
        }
        Some(_) => Err(bad(format!("{k} must be an array of strings"))),
    }
}

/// `env` as `{K: "V" | null}`; null unsets.
fn take_env(
    args: &mut Map<String, Value>,
    k: &str,
) -> Result<BTreeMap<String, Option<String>>, ErrorBody> {
    match args.remove(k) {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(v) => serde_json::from_value(v)
            .map_err(|_| bad(format!("{k} must be an object of strings (null unsets)"))),
    }
}

fn reject_rest(tool: &str, args: &Map<String, Value>) -> Result<(), ErrorBody> {
    match args.keys().next() {
        Some(k) => Err(bad(format!("{tool}: unknown argument {k:?}"))),
        None => Ok(()),
    }
}

fn tools() -> Value {
    let session = json!({ "type": "string", "description": "Session id; optional when exactly one session is running." });
    let command = json!({
        "type": "array", "items": { "type": "string" },
        "description": "Program and arguments, e.g. [\"cargo\", \"run\", \"--example\", \"hello\"]."
    });
    let env = json!({
        "type": "object", "additionalProperties": { "type": ["string", "null"] },
        "description": "Environment overrides for the app; null unsets a variable."
    });
    let region = json!({
        "type": "array", "items": { "type": "number" }, "minItems": 4, "maxItems": 4,
        "description": "[x0, y0, x1, y1]: top-left and bottom-right corners in screenshot pixels."
    });
    json!([
        {
            "name": "start",
            "title": "Start Desktop",
            "annotations": { "title": "Start Desktop", "destructiveHint": false, "openWorldHint": false },
            "description": "Start a headless Wayland desktop session, optionally launching an app in it. \
                With `command`, waits for the app's first window and returns a screenshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": command,
                    "env": env,
                    "resolution": { "type": "string", "description": "Logical size WxH (default 1280x800)." },
                    "scale": { "type": "number", "description": "Output scale, e.g. 1, 1.5, 2 (default 1)." },
                    "mobile": { "type": "boolean", "description": "390x844 at scale 3 with fullscreen windows." },
                    "window_mode": { "type": "string", "enum": ["maximized", "fullscreen", "floating"] },
                    "gpu": { "type": "string", "enum": ["auto", "software"] },
                    "home": { "type": "string", "description": "isolated (default), host, or a directory." },
                    "dbus": { "type": "string", "enum": ["private", "host", "none"] },
                    "keymap": { "type": "string", "description": "xkb LAYOUT[:VARIANT] (default us)." },
                    "exit_with_app": { "type": "boolean" },
                    "screenshot": { "type": "boolean", "description": "Return a screenshot after the window appears (default true)." },
                    "timeout_ms": { "type": "integer", "description": "How long to wait for the first window (default 10000)." }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "launch",
            "title": "Launch App",
            "annotations": { "title": "Launch App", "destructiveHint": false, "openWorldHint": false },
            "description": "Launch an app in an existing session (one app per session), wait for its first window and return a screenshot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "command": command,
                    "env": env,
                    "cwd": { "type": "string" },
                    "screenshot": { "type": "boolean" },
                    "timeout_ms": { "type": "integer" }
                },
                "required": ["command"],
                "additionalProperties": false
            }
        },
        {
            "name": "computer",
            "title": "Computer Use",
            "annotations": { "title": "Computer Use", "openWorldHint": false },
            "description": "Drive the session with Anthropic computer-use actions: either one action \
                (`action` plus its fields, e.g. {\"action\":\"left_click\",\"coordinate\":[100,200]}) or a batch \
                (`actions`: [{\"type\":\"left_click\",...}, ...]) executed in order, stopping at the first failure. \
                Actions: screenshot, zoom, left_click, right_click, middle_click, double_click, triple_click, \
                left_mouse_down, left_mouse_up, mouse_move, left_click_drag, cursor_position, scroll, type, key, \
                hold_key, wait; extensions vway_wait, vway_touch, vway_gesture (tap, long_press, swipe, pinch), \
                vway_resize, vway_key. Coordinates are pixels of the screenshots this tool returns, also after a \
                zoom; region is [x0, y0, x1, y1]; durations of wait/hold_key are seconds; key text is a combo \
                like \"ctrl+s\" or several separated by spaces.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": session,
                    "actions": { "type": "array", "items": { "type": "object" } },
                    "action": { "type": "string" },
                    "coordinate": { "type": "array", "items": { "type": "number" } },
                    "start_coordinate": { "type": "array", "items": { "type": "number" } },
                    "text": { "type": "string" },
                    "scroll_direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                    "scroll_amount": { "type": "number" },
                    "duration": { "type": "number" },
                    "region": region
                },
                "additionalProperties": true
            }
        },
        {
            "name": "screenshot",
            "title": "Screenshot",
            "annotations": { "title": "Screenshot", "readOnlyHint": true, "openWorldHint": false },
            "description": "Screenshot the session (waits briefly for the app to go idle unless settle is false).",
            "inputSchema": {
                "type": "object",
                "properties": { "session": session, "region": region, "settle": { "type": "boolean" } },
                "additionalProperties": false
            }
        },
        {
            "name": "status",
            "title": "Session Status",
            "annotations": { "title": "Session Status", "readOnlyHint": true, "openWorldHint": false },
            "description": "Session status: output, pointer, windows (with ids for focus), app state, timing.",
            "inputSchema": { "type": "object", "properties": { "session": session }, "additionalProperties": false }
        },
        {
            "name": "stop",
            "title": "Stop Session",
            "annotations": { "title": "Stop Session", "destructiveHint": true, "idempotentHint": true, "openWorldHint": false },
            "description": "Stop a session and its app.",
            "inputSchema": { "type": "object", "properties": { "session": session }, "additionalProperties": false }
        },
        {
            "name": "list",
            "title": "List Sessions",
            "annotations": { "title": "List Sessions", "readOnlyHint": true, "openWorldHint": false },
            "description": "List running sessions (including ones started outside this server).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_extracted() {
        let mut v = json!({ "results": [ { "result": { "png_base64": "AAAA", "width": 1 } } ] });
        let mut images = Vec::new();
        extract_images(&mut v, &mut images);
        assert_eq!(images.len(), 1);
        assert_eq!(v["results"][0]["result"]["image"], json!(1));
        assert!(v["results"][0]["result"].get("png_base64").is_none());
    }

    #[test]
    fn rpc_basics() {
        let mut s = Server::new(PathBuf::from("/nonexistent"), vec![], ImageLimit::Standard);
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#)
            .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert!(
            s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .unwrap();
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 7);
        let r = s
            .handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#)
            .unwrap();
        assert_eq!(r["error"]["code"], -32601);
        let r = s
            .handle_line(
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"status"}}"#,
            )
            .unwrap();
        assert_eq!(r["result"]["isError"], true);
    }

    #[test]
    fn allow_list() {
        let sh = resolve_program("sh").expect("sh on PATH");
        let dir = format!("{}/", sh.parent().unwrap().display());
        let s = Server::new(PathBuf::from("/x"), vec![dir], ImageLimit::Standard);
        let none = BTreeMap::new();
        assert_eq!(
            s.check_allowed(&["sh".into()], &none).unwrap()[0],
            sh.to_string_lossy()
        );
        let s = Server::new(
            PathBuf::from("/x"),
            vec!["/nonexistent/".into(), "true".into()],
            ImageLimit::Standard,
        );
        assert!(s.check_allowed(&["true".into()], &none).is_ok());
        assert_eq!(
            s.check_allowed(&["sh".into()], &none).unwrap_err().code,
            ErrorCode::NotAllowed
        );
        let path = BTreeMap::from([("PATH".to_string(), Some("/tmp".to_string()))]);
        assert!(s.check_allowed(&["true".into()], &path).is_err());
    }
}
