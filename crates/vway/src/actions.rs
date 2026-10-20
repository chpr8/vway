//! `vway.action.v1` runner (design 0009): Anthropic computer-use action names plus `vway_*`
//! extensions, executed in order with stop-at-first-failure semantics.
//!
//! In [`Space::Screenshot`] (scripts, MCP, `vway computer`) every coordinate is a pixel of the
//! screenshots the runner returns, as in Anthropic computer use. Screenshots are the logical size
//! of the output, shrunk to fit an [`ImageLimit`]. [`Space::Logical`] (the low-level CLI
//! commands) uses logical pixels and full physical-resolution screenshots.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value, json};
use vway_proto::{
    Button, ButtonAction, ErrorBody, ErrorCode, KeyAction, Op, TouchEvent, TouchPhase, WaitFor, err,
};

use crate::client::Client;

const STEP: Duration = Duration::from_millis(16);

/// Reported for every action after the first failure (Anthropic's batch halt text).
pub const NOT_EXECUTED: &str = "Not executed: an earlier computer action in this turn failed.";

/// The largest screenshot a model accepts without the API shrinking or rejecting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageLimit {
    /// 1568 px on the long edge, 1.15 MP: safe for every Claude model.
    Standard,
    /// 2576 px on the long edge, 4784 visual tokens (≈3.75 MP): Claude Opus 4.7 and later,
    /// including every model that takes `computer_toolset_20260801`.
    Large,
    /// No limit: screenshots are the logical size of the output.
    None,
}

impl ImageLimit {
    /// The largest size within the limit with the aspect ratio of `w`×`h`, never larger than it.
    pub fn fit(self, w: f64, h: f64) -> (u32, u32) {
        let (w, h) = (w.round().max(1.0), h.round().max(1.0));
        let (long, area) = match self {
            ImageLimit::Standard => (1568.0, 1_150_000.0),
            ImageLimit::Large => (2576.0, 4784.0 * 28.0 * 28.0),
            ImageLimit::None => return (w as u32, h as u32),
        };
        let mut k = (long / w.max(h)).min((area / (w * h)).sqrt()).min(1.0);
        loop {
            let (fw, fh) = ((w * k).floor().max(1.0), (h * k).floor().max(1.0));
            let tokens = (fw / 28.0).ceil() * (fh / 28.0).ceil();
            if self != ImageLimit::Large || tokens <= 4784.0 {
                return (fw as u32, fh as u32);
            }
            k *= 0.995;
        }
    }
}

/// The unit of action coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    /// Logical pixels; screenshots at physical resolution.
    Logical,
    /// Pixels of the screenshots this runner returns.
    Screenshot(ImageLimit),
}

/// The output as screenshots see it.
#[derive(Debug, Clone, Copy)]
struct Frame {
    /// Logical size.
    lw: f64,
    lh: f64,
    scale: f64,
    /// Screenshot size (equal to the logical size in [`Space::Logical`]).
    w: f64,
    h: f64,
}

impl Frame {
    fn kx(&self) -> f64 {
        self.w / self.lw
    }
    fn ky(&self) -> f64 {
        self.h / self.lh
    }
}

/// Where screenshots go.
#[derive(Debug, Clone)]
pub enum Images {
    /// Written as PNG files into this directory (CLI).
    Files(PathBuf),
    /// Returned inline as base64 (MCP).
    Inline,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionResult {
    pub index: usize,
    #[serde(rename = "type")]
    pub kind: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub ok: bool,
    pub results: Vec<ActionResult>,
}

/// Accept `{"vway_action": 1, "actions": [...]}` or a bare array.
pub fn parse_script(v: &Value) -> Result<Vec<Value>, ErrorBody> {
    let bad = |m: &str| err(ErrorCode::ScriptError, m.to_string());
    match v {
        Value::Array(a) => Ok(a.clone()),
        Value::Object(o) => {
            if let Some(ver) = o.get("vway_action")
                && ver != &json!(1)
            {
                return Err(bad("unsupported vway_action version (expected 1)"));
            }
            for k in o.keys() {
                if k != "vway_action" && k != "actions" {
                    return Err(bad(&format!("unknown script field {k:?}")));
                }
            }
            match o.get("actions") {
                Some(Value::Array(a)) => Ok(a.clone()),
                _ => Err(bad("script needs an \"actions\" array")),
            }
        }
        _ => Err(bad("script must be an object or an array of actions")),
    }
}

fn allowed_fields(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "screenshot" => &["region", "path", "settle"],
        "zoom" => &["region", "path"],
        "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" => {
            &["coordinate", "text"]
        }
        "left_mouse_down" | "left_mouse_up" => &["coordinate"],
        "mouse_move" => &["coordinate", "duration_ms"],
        "left_click_drag" => &["start_coordinate", "coordinate", "duration_ms", "text"],
        "cursor_position" => &[],
        "scroll" => &["scroll_direction", "scroll_amount", "coordinate", "text"],
        "type" => &["text"],
        "key" => &["text", "repeat", "modifiers"],
        "hold_key" => &["text", "duration"],
        "wait" => &["duration"],
        "vway_wait" => &[
            "for",
            "timeout_ms",
            "quiet_ms",
            "title",
            "app_id",
            "after_seq",
        ],
        "vway_touch" => &["events"],
        "vway_gesture" => &[
            "kind",
            "coordinate",
            "start_coordinate",
            "duration_ms",
            "from_distance",
            "to_distance",
        ],
        "vway_resize" => &["width", "height", "scale"],
        "vway_key" => &["keys", "action"],
        _ => return None,
    })
}

struct Args<'a> {
    kind: &'a str,
    o: &'a Map<String, Value>,
}

impl<'a> Args<'a> {
    fn bad(&self, m: String) -> ErrorBody {
        err(ErrorCode::ScriptError, format!("{}: {m}", self.kind))
    }

    fn get(&self, k: &str) -> Option<&'a Value> {
        self.o.get(k).filter(|v| !v.is_null())
    }

    fn f64_opt(&self, k: &str) -> Result<Option<f64>, ErrorBody> {
        match self.get(k) {
            None => Ok(None),
            Some(v) => v
                .as_f64()
                .map(Some)
                .ok_or_else(|| self.bad(format!("{k} must be a number"))),
        }
    }

    fn f64(&self, k: &str) -> Result<f64, ErrorBody> {
        self.f64_opt(k)?
            .ok_or_else(|| self.bad(format!("missing {k}")))
    }

    fn u64_opt(&self, k: &str) -> Result<Option<u64>, ErrorBody> {
        match self.get(k) {
            None => Ok(None),
            Some(v) => v
                .as_u64()
                .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f.round() as u64))
                .map(Some)
                .ok_or_else(|| self.bad(format!("{k} must be a non-negative number"))),
        }
    }

    fn str_opt(&self, k: &str) -> Result<Option<&'a str>, ErrorBody> {
        match self.get(k) {
            None => Ok(None),
            Some(v) => v
                .as_str()
                .map(Some)
                .ok_or_else(|| self.bad(format!("{k} must be a string"))),
        }
    }

    fn str(&self, k: &str) -> Result<&'a str, ErrorBody> {
        self.str_opt(k)?
            .ok_or_else(|| self.bad(format!("missing {k}")))
    }

    fn coord_opt(&self, k: &str) -> Result<Option<(f64, f64)>, ErrorBody> {
        match self.get(k) {
            None => Ok(None),
            Some(Value::Array(a)) if a.len() == 2 => match (a[0].as_f64(), a[1].as_f64()) {
                (Some(x), Some(y)) => Ok(Some((x, y))),
                _ => Err(self.bad(format!("{k} must be [x, y] numbers"))),
            },
            Some(_) => Err(self.bad(format!("{k} must be [x, y]"))),
        }
    }

    fn coord(&self, k: &str) -> Result<(f64, f64), ErrorBody> {
        self.coord_opt(k)?
            .ok_or_else(|| self.bad(format!("missing {k}")))
    }

    /// `region`: `[x0, y0, x1, y1]`, the top-left and bottom-right corners.
    fn region_opt(&self) -> Result<Option<[f64; 4]>, ErrorBody> {
        let shape = "region must be [x0, y0, x1, y1] (top-left and bottom-right corners)";
        match self.get("region") {
            None => Ok(None),
            Some(Value::Array(a)) if a.len() == 4 => {
                let mut r = [0.0; 4];
                for (i, v) in a.iter().enumerate() {
                    r[i] = v.as_f64().ok_or_else(|| self.bad(shape.into()))?;
                }
                if r[2] <= r[0] || r[3] <= r[1] {
                    return Err(self.bad(format!(
                        "{shape}; x1 must exceed x0 and y1 must exceed y0, got {r:?}"
                    )));
                }
                Ok(Some(r))
            }
            Some(_) => Err(self.bad(shape.into())),
        }
    }
}

pub struct Runner<'a> {
    client: &'a mut Client,
    images: Images,
    space: Space,
    frame: Option<Frame>,
    shot_seq: u32,
}

impl<'a> Runner<'a> {
    pub fn new(client: &'a mut Client, images: Images, space: Space) -> Self {
        Runner {
            client,
            images,
            space,
            frame: None,
            shot_seq: 0,
        }
    }

    fn frame(&mut self) -> Result<Frame, ErrorBody> {
        if let Some(f) = self.frame {
            return Ok(f);
        }
        let o = self.client.status()?.output;
        let (lw, lh) = (o.width as f64, o.height as f64);
        let (w, h) = match self.space {
            Space::Logical => (lw, lh),
            Space::Screenshot(limit) => {
                let (w, h) = limit.fit(lw, lh);
                (w as f64, h as f64)
            }
        };
        let f = Frame {
            lw,
            lh,
            scale: o.scale,
            w,
            h,
        };
        self.frame = Some(f);
        Ok(f)
    }

    /// An action coordinate in logical pixels.
    fn logical(&mut self, (x, y): (f64, f64)) -> Result<(f64, f64), ErrorBody> {
        if self.space == Space::Logical {
            return Ok((x, y));
        }
        let f = self.frame()?;
        Ok((x / f.kx(), y / f.ky()))
    }

    fn coord_opt(&mut self, args: &Args, k: &str) -> Result<Option<(f64, f64)>, ErrorBody> {
        match args.coord_opt(k)? {
            Some(p) => self.logical(p).map(Some),
            None => Ok(None),
        }
    }

    fn coord(&mut self, args: &Args, k: &str) -> Result<(f64, f64), ErrorBody> {
        let p = args.coord(k)?;
        self.logical(p)
    }

    /// A horizontal action distance in logical pixels.
    fn length(&mut self, d: f64) -> Result<f64, ErrorBody> {
        if self.space == Space::Logical {
            return Ok(d);
        }
        Ok(d / self.frame()?.kx())
    }

    pub fn run(&mut self, actions: &[Value]) -> Report {
        let mut results = Vec::with_capacity(actions.len());
        let mut failed = false;
        for (index, a) in actions.iter().enumerate() {
            let kind = a
                .get("type")
                .or_else(|| a.get("action"))
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            if failed {
                results.push(ActionResult {
                    index,
                    kind,
                    ok: false,
                    result: None,
                    error: Some(err(ErrorCode::NotExecuted, NOT_EXECUTED)),
                });
                continue;
            }
            let r = self.run_one(a);
            failed = r.is_err();
            results.push(match r {
                Ok(v) => ActionResult {
                    index,
                    kind,
                    ok: true,
                    result: Some(v),
                    error: None,
                },
                Err(e) => ActionResult {
                    index,
                    kind,
                    ok: false,
                    result: None,
                    error: Some(e),
                },
            });
        }
        Report {
            ok: !failed,
            results,
        }
    }

    pub fn run_one(&mut self, a: &Value) -> Result<Value, ErrorBody> {
        let o = a
            .as_object()
            .ok_or_else(|| err(ErrorCode::ScriptError, "each action must be an object"))?;
        let kind = o
            .get("type")
            .or_else(|| o.get("action"))
            .and_then(Value::as_str)
            .ok_or_else(|| err(ErrorCode::ScriptError, "action needs a \"type\""))?;
        let allowed = allowed_fields(kind).ok_or_else(|| {
            err(
                ErrorCode::ScriptError,
                format!("unknown action type {kind:?}"),
            )
        })?;
        for k in o.keys() {
            if k != "type" && k != "action" && !allowed.contains(&k.as_str()) {
                return Err(err(
                    ErrorCode::ScriptError,
                    format!(
                        "{kind}: unknown field {k:?} (allowed: {})",
                        allowed.join(", ")
                    ),
                ));
            }
        }
        let args = Args { kind, o };
        match kind {
            "screenshot" => {
                let settle = match args.get("settle") {
                    None => true,
                    Some(v) => v
                        .as_bool()
                        .ok_or_else(|| args.bad("settle must be a boolean".into()))?,
                };
                if settle {
                    self.settle()?;
                }
                self.screenshot(args.region_opt()?, false, args.str_opt("path")?)
            }
            "zoom" => {
                let region = args
                    .region_opt()?
                    .ok_or_else(|| args.bad("missing region".into()))?;
                self.settle()?;
                self.screenshot(Some(region), true, args.str_opt("path")?)
            }
            "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" => {
                let (button, count) = match kind {
                    "right_click" => ("right", 1),
                    "middle_click" => ("middle", 1),
                    "double_click" => ("left", 2),
                    "triple_click" => ("left", 3),
                    _ => ("left", 1),
                };
                if let Some((x, y)) = self.coord_opt(&args, "coordinate")? {
                    self.move_to(x, y)?;
                }
                let mods = args.str_opt("text")?;
                self.with_modifiers(mods, |r| {
                    r.client.call(&Op::PointerButton {
                        button: Button::Name(button.into()),
                        action: ButtonAction::Click,
                        count: Some(count),
                    })
                })
            }
            "left_mouse_down" | "left_mouse_up" => {
                if let Some((x, y)) = self.coord_opt(&args, "coordinate")? {
                    self.move_to(x, y)?;
                }
                self.client.call(&Op::PointerButton {
                    button: Button::Name("left".into()),
                    action: if kind == "left_mouse_down" {
                        ButtonAction::Down
                    } else {
                        ButtonAction::Up
                    },
                    count: None,
                })
            }
            "mouse_move" => {
                let (x, y) = self.coord(&args, "coordinate")?;
                let d = args.u64_opt("duration_ms")?.unwrap_or(0);
                self.glide(x, y, d)
            }
            "left_click_drag" => {
                let (sx, sy) = self.coord(&args, "start_coordinate")?;
                let (x, y) = self.coord(&args, "coordinate")?;
                let d = args.u64_opt("duration_ms")?.unwrap_or(300);
                let mods = args.str_opt("text")?;
                self.move_to(sx, sy)?;
                self.with_modifiers(mods, |r| {
                    r.button("left", ButtonAction::Down)?;
                    let moved = r.glide(x, y, d);
                    let up = r.button("left", ButtonAction::Up);
                    moved?;
                    up
                })
            }
            "cursor_position" => {
                let p = self.client.status()?.pointer;
                if self.space == Space::Logical {
                    return Ok(json!({ "x": p.x, "y": p.y }));
                }
                let f = self.frame()?;
                Ok(json!({ "x": (p.x * f.kx()).round(), "y": (p.y * f.ky()).round() }))
            }
            "scroll" => {
                let dir = args.str("scroll_direction")?;
                let amount = args.f64_opt("scroll_amount")?.unwrap_or(3.0);
                let (dx, dy) = match dir {
                    "up" => (0.0, -amount),
                    "down" => (0.0, amount),
                    "left" => (-amount, 0.0),
                    "right" => (amount, 0.0),
                    other => {
                        return Err(args.bad(format!(
                            "scroll_direction {other:?} must be up, down, left or right"
                        )));
                    }
                };
                let at = self.coord_opt(&args, "coordinate")?;
                let mods = args.str_opt("text")?;
                self.with_modifiers(mods, |r| {
                    r.client.call(&Op::Scroll {
                        dx,
                        dy,
                        x: at.map(|p| p.0),
                        y: at.map(|p| p.1),
                    })
                })
            }
            "type" => self.client.call(&Op::Type {
                text: args.str("text")?.to_string(),
            }),
            "key" => {
                let text = args.str("text")?;
                let repeat = args.u64_opt("repeat")?.map(|r| r as u32);
                // xdotool style: several keys separated by spaces, pressed in order.
                let seq: Vec<&str> = text.split_whitespace().collect();
                if seq.len() > 1 {
                    if args.get("modifiers").is_some() {
                        return Err(args.bad("modifiers needs a single key in text".into()));
                    }
                    let mut last = json!({});
                    for _ in 0..repeat.unwrap_or(1) {
                        for k in &seq {
                            last = self.key(k, KeyAction::Press)?;
                        }
                    }
                    return Ok(last);
                }
                let keys = match args.get("modifiers") {
                    None => text.to_string(),
                    Some(Value::Array(m)) => {
                        let mut parts: Vec<String> = m
                            .iter()
                            .map(|v| {
                                v.as_str()
                                    .map(String::from)
                                    .ok_or_else(|| args.bad("modifiers must be strings".into()))
                            })
                            .collect::<Result<_, _>>()?;
                        parts.push(text.to_string());
                        parts.join("+")
                    }
                    Some(_) => return Err(args.bad("modifiers must be an array".into())),
                };
                self.client.call(&Op::Key {
                    keys: Some(keys),
                    code: None,
                    action: KeyAction::Press,
                    repeat,
                })
            }
            "hold_key" => {
                let text = args.str("text")?;
                let secs = args.f64("duration")?;
                if !(0.0..=600.0).contains(&secs) {
                    return Err(args.bad("duration must be 0..600 seconds".into()));
                }
                let seq: Vec<&str> = text.split_whitespace().collect();
                let mut held = Vec::new();
                let mut down = Ok(json!({}));
                for k in &seq {
                    down = self.key(k, KeyAction::Down);
                    if down.is_err() {
                        break;
                    }
                    held.push(*k);
                }
                if down.is_ok() {
                    std::thread::sleep(Duration::from_secs_f64(secs));
                }
                let mut up = Ok(json!({}));
                for k in held.iter().rev() {
                    let r = self.key(k, KeyAction::Up);
                    if up.is_ok() {
                        up = r;
                    }
                }
                down?;
                if seq.is_empty() {
                    return Err(args.bad("missing key in text".into()));
                }
                up
            }
            "wait" => {
                let secs = args.f64("duration")?;
                if !(0.0..=600.0).contains(&secs) {
                    return Err(args.bad("duration must be 0..600 seconds".into()));
                }
                std::thread::sleep(Duration::from_secs_f64(secs));
                Ok(json!({}))
            }
            "vway_wait" => {
                let cond: WaitFor =
                    serde_json::from_value(args.get("for").cloned().unwrap_or(json!("idle")))
                        .map_err(|_| {
                            args.bad("for must be window, frame, idle or app_exit".into())
                        })?;
                self.client.call(&Op::Wait {
                    condition: cond,
                    timeout_ms: args.u64_opt("timeout_ms")?,
                    title: args.str_opt("title")?.map(String::from),
                    app_id: args.str_opt("app_id")?.map(String::from),
                    after_seq: args.u64_opt("after_seq")?,
                    quiet_ms: args.u64_opt("quiet_ms")?,
                })
            }
            "vway_touch" => {
                let Some(Value::Array(evs)) = args.get("events") else {
                    return Err(args.bad("events must be an array".into()));
                };
                let mut events = Vec::with_capacity(evs.len());
                for ev in evs {
                    let eo = ev
                        .as_object()
                        .ok_or_else(|| args.bad("each event must be an object".into()))?;
                    let ea = Args { kind, o: eo };
                    let phase: TouchPhase =
                        serde_json::from_value(eo.get("phase").cloned().unwrap_or(Value::Null))
                            .map_err(|_| {
                                args.bad("phase must be down, move, up or cancel".into())
                            })?;
                    let id = ea.u64_opt("id")?.unwrap_or(0) as u32;
                    let at = self.coord_opt(&ea, "coordinate")?;
                    events.push(TouchEvent {
                        phase,
                        id,
                        x: at.map(|p| p.0),
                        y: at.map(|p| p.1),
                    });
                }
                self.client.call(&Op::Touch { events })
            }
            "vway_gesture" => self.gesture(&args),
            "vway_resize" => {
                self.frame = None;
                self.client.call(&Op::Resize {
                    width: args.f64("width")? as i32,
                    height: args.f64("height")? as i32,
                    scale: args.f64_opt("scale")?,
                })
            }
            "vway_key" => {
                let action: KeyAction =
                    serde_json::from_value(args.get("action").cloned().unwrap_or(json!("press")))
                        .map_err(|_| args.bad("action must be press, down or up".into()))?;
                self.key(args.str("keys")?, action)
            }
            _ => unreachable!("checked by allowed_fields"),
        }
    }

    /// Bounded wait for the app to settle before a capture; timeouts are not errors.
    fn settle(&mut self) -> Result<(), ErrorBody> {
        match self.client.call(&Op::Wait {
            condition: WaitFor::Idle,
            timeout_ms: Some(3000),
            title: None,
            app_id: None,
            after_seq: None,
            quiet_ms: None,
        }) {
            Ok(_) => Ok(()),
            Err(e) if matches!(e.code, ErrorCode::Timeout | ErrorCode::AppExited) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// `region` holds action-space corners. A zoom returns the region at the output's full
    /// resolution (within the image limit); a screenshot returns it at screenshot scale.
    fn screenshot(
        &mut self,
        region: Option<[f64; 4]>,
        zoom: bool,
        path: Option<&str>,
    ) -> Result<Value, ErrorBody> {
        let (region, size) = match (self.space, region) {
            (Space::Logical, r) => (r.map(|[x0, y0, x1, y1]| [x0, y0, x1 - x0, y1 - y0]), None),
            (Space::Screenshot(_), None) => {
                let f = self.frame()?;
                (None, Some([f.w as u32, f.h as u32]))
            }
            (Space::Screenshot(limit), Some([x0, y0, x1, y1])) => {
                let f = self.frame()?;
                let (x0, y0) = (x0.max(0.0), y0.max(0.0));
                let (x1, y1) = (x1.min(f.w), y1.min(f.h));
                if x1 <= x0 || y1 <= y0 {
                    return Err(err(
                        ErrorCode::OutOfRange,
                        format!("region lies outside the {}x{} screenshot", f.w, f.h),
                    ));
                }
                let (kx, ky) = (f.kx(), f.ky());
                let logical = [x0 / kx, y0 / ky, (x1 - x0) / kx, (y1 - y0) / ky];
                let (w, h) = if zoom {
                    limit.fit(logical[2] * f.scale, logical[3] * f.scale)
                } else {
                    (
                        (x1 - x0).round().max(1.0) as u32,
                        (y1 - y0).round().max(1.0) as u32,
                    )
                };
                (Some(logical), Some([w, h]))
            }
        };
        let path = match (path, &self.images) {
            (Some(p), _) => Some(absolute(Path::new(p))),
            (None, Images::Files(dir)) => {
                self.shot_seq += 1;
                std::fs::create_dir_all(dir).map_err(|e| {
                    err(
                        ErrorCode::IoError,
                        format!("cannot create {}: {e}", dir.display()),
                    )
                })?;
                Some(absolute(
                    &dir.join(format!("shot-{:03}.png", self.shot_seq)),
                ))
            }
            (None, Images::Inline) => None,
        };
        self.client.call(&Op::Screenshot {
            region,
            path: path.map(|p| p.to_string_lossy().into_owned()),
            size,
        })
    }

    fn move_to(&mut self, x: f64, y: f64) -> Result<Value, ErrorBody> {
        self.client.call(&Op::PointerMove { x, y })
    }

    fn button(&mut self, name: &str, action: ButtonAction) -> Result<Value, ErrorBody> {
        self.client.call(&Op::PointerButton {
            button: Button::Name(name.into()),
            action,
            count: None,
        })
    }

    fn key(&mut self, keys: &str, action: KeyAction) -> Result<Value, ErrorBody> {
        self.client.call(&Op::Key {
            keys: Some(keys.to_string()),
            code: None,
            action,
            repeat: None,
        })
    }

    /// Hold `mods` (e.g. `"ctrl+shift"`) around `f`, releasing them even if `f` fails.
    fn with_modifiers(
        &mut self,
        mods: Option<&str>,
        f: impl FnOnce(&mut Self) -> Result<Value, ErrorBody>,
    ) -> Result<Value, ErrorBody> {
        let mods: Vec<String> = mods
            .map(|m| {
                m.split('+')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let mut held: Vec<&str> = Vec::new();
        for m in &mods {
            if let Err(e) = self.key(m, KeyAction::Down) {
                for h in held.iter().rev() {
                    let _ = self.key(h, KeyAction::Up);
                }
                return Err(e);
            }
            held.push(m);
        }
        let r = f(self);
        for h in held.iter().rev() {
            let _ = self.key(h, KeyAction::Up);
        }
        r
    }

    /// Move the pointer to (x, y), interpolating over `duration_ms`.
    fn glide(&mut self, x: f64, y: f64, duration_ms: u64) -> Result<Value, ErrorBody> {
        let steps = (duration_ms / STEP.as_millis() as u64).max(1);
        if steps == 1 {
            return self.move_to(x, y);
        }
        let start = self.client.status()?.pointer;
        let mut last = json!({});
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            last = self.move_to(start.x + (x - start.x) * t, start.y + (y - start.y) * t)?;
            if i < steps {
                std::thread::sleep(STEP);
            }
        }
        Ok(last)
    }

    fn touch(&mut self, events: Vec<TouchEvent>) -> Result<Value, ErrorBody> {
        self.client.call(&Op::Touch { events })
    }

    fn gesture(&mut self, args: &Args) -> Result<Value, ErrorBody> {
        let ev = |phase, id, p: Option<(f64, f64)>| TouchEvent {
            phase,
            id,
            x: p.map(|p| p.0),
            y: p.map(|p| p.1),
        };
        let kind = args.str("kind")?;
        match kind {
            "tap" | "long_press" => {
                let p = self.coord(args, "coordinate")?;
                let hold =
                    args.u64_opt("duration_ms")?
                        .unwrap_or(if kind == "tap" { 50 } else { 800 });
                self.touch(vec![ev(TouchPhase::Down, 0, Some(p))])?;
                std::thread::sleep(Duration::from_millis(hold));
                self.touch(vec![ev(TouchPhase::Up, 0, None)])
            }
            "swipe" => {
                let (sx, sy) = self.coord(args, "start_coordinate")?;
                let (x, y) = self.coord(args, "coordinate")?;
                let d = args.u64_opt("duration_ms")?.unwrap_or(300);
                let steps = (d / STEP.as_millis() as u64).max(1);
                self.touch(vec![ev(TouchPhase::Down, 0, Some((sx, sy)))])?;
                let mut moved = Ok(json!({}));
                for i in 1..=steps {
                    std::thread::sleep(STEP);
                    let t = i as f64 / steps as f64;
                    moved = self.touch(vec![ev(
                        TouchPhase::Move,
                        0,
                        Some((sx + (x - sx) * t, sy + (y - sy) * t)),
                    )]);
                    if moved.is_err() {
                        break;
                    }
                }
                let up = self.touch(vec![ev(TouchPhase::Up, 0, None)]);
                moved?;
                up
            }
            "pinch" => {
                let (cx, cy) = self.coord(args, "coordinate")?;
                let from = self.length(args.f64_opt("from_distance")?.unwrap_or(200.0))?;
                let to = self.length(args.f64_opt("to_distance")?.unwrap_or(80.0))?;
                let d = args.u64_opt("duration_ms")?.unwrap_or(400);
                let steps = (d / STEP.as_millis() as u64).max(1);
                let fingers = |dist: f64, phase| {
                    vec![
                        ev(phase, 0, Some((cx - dist / 2.0, cy))),
                        ev(phase, 1, Some((cx + dist / 2.0, cy))),
                    ]
                };
                self.touch(fingers(from, TouchPhase::Down))?;
                let mut moved = Ok(json!({}));
                for i in 1..=steps {
                    std::thread::sleep(STEP);
                    let t = i as f64 / steps as f64;
                    moved = self.touch(fingers(from + (to - from) * t, TouchPhase::Move));
                    if moved.is_err() {
                        break;
                    }
                }
                let up = self.touch(vec![
                    ev(TouchPhase::Up, 0, None),
                    ev(TouchPhase::Up, 1, None),
                ]);
                moved?;
                up
            }
            other => Err(args.bad(format!(
                "kind {other:?} must be tap, long_press, swipe or pinch"
            ))),
        }
    }
}

pub fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_envelopes() {
        assert_eq!(
            parse_script(&json!([{"type": "wait", "duration": 0}]))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            parse_script(&json!({"vway_action": 1, "actions": [{"type": "type", "text": "x"}]}))
                .unwrap()
                .len(),
            1
        );
        assert!(parse_script(&json!({"vway_action": 2, "actions": []})).is_err());
        assert!(parse_script(&json!({"actions": [], "extra": 1})).is_err());
        assert!(parse_script(&json!("nope")).is_err());
    }

    #[test]
    fn field_tables_cover_documented_actions() {
        for k in [
            "screenshot",
            "zoom",
            "left_click",
            "right_click",
            "middle_click",
            "double_click",
            "triple_click",
            "left_mouse_down",
            "left_mouse_up",
            "mouse_move",
            "left_click_drag",
            "cursor_position",
            "scroll",
            "type",
            "key",
            "hold_key",
            "wait",
            "vway_wait",
            "vway_touch",
            "vway_gesture",
            "vway_resize",
            "vway_key",
        ] {
            assert!(allowed_fields(k).is_some(), "{k}");
        }
        assert!(allowed_fields("bogus").is_none());
    }

    #[test]
    fn image_limits() {
        use ImageLimit::*;
        assert_eq!(Standard.fit(1280.0, 800.0), (1280, 800));
        assert_eq!(Standard.fit(1920.0, 1080.0), (1429, 804));
        assert_eq!(Large.fit(1920.0, 1080.0), (1920, 1080));
        assert_eq!(None.fit(3840.0, 2160.0), (3840, 2160));
        for (w, h) in [(3840.0, 2160.0), (2560.0, 2560.0), (5000.0, 400.0)] {
            let (fw, fh) = Large.fit(w, h);
            let tokens = fw.div_ceil(28) * fh.div_ceil(28);
            assert!(fw.max(fh) <= 2576 && tokens <= 4784, "{w}x{h} -> {fw}x{fh}");
            let (fw, fh) = Standard.fit(w, h);
            assert!(
                fw.max(fh) <= 1568 && fw * fh <= 1_150_000,
                "{w}x{h} -> {fw}x{fh}"
            );
            assert!(((fw as f64 / fh as f64) - w / h).abs() / (w / h) < 0.02);
        }
    }
}
