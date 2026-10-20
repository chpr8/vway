//! `vway computer`: answer Claude's computer-use tool calls (design 0009 §A).
//!
//! A harness that declares Anthropic's own computer tool passes the `tool_use` blocks of a response
//! through unchanged and appends the returned `tool_result` blocks to the next user message. Both
//! request shapes are understood:
//!
//! - `computer_toolset_20260801`: one block per action, `name` is the action, `toolset_name` is
//!   `"computer"`; results echo `toolset_name`.
//! - `computer_20251124` / `computer_20250124`: `name` is `"computer"`, the action is
//!   `input.action`.
//!
//! Blocks run in order; after the first failure the rest are answered with the halt text.

use serde_json::{Map, Value, json};
use vway_proto::{ErrorBody, ErrorCode, err};

use crate::actions::{ImageLimit, NOT_EXECUTED, Runner};

/// One computer-use call taken from a `tool_use` block.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub id: String,
    /// From `computer_toolset_20260801` (results carry `toolset_name`).
    pub toolset: bool,
    /// The call as a `vway.action.v1` action.
    pub action: Value,
}

/// The computer-use calls in `input`: a `tool_use` block, an array of content blocks, or a message
/// or API response with `content`. Other blocks (text, thinking, other tools) are skipped.
pub fn calls(input: &Value) -> Result<Vec<Call>, ErrorBody> {
    let bad = |m: String| err(ErrorCode::ScriptError, m);
    let blocks: Vec<&Value> = match input {
        Value::Array(a) => a.iter().collect(),
        Value::Object(o) => match o.get("content") {
            Some(Value::Array(a)) => a.iter().collect(),
            _ => vec![input],
        },
        _ => {
            return Err(bad(
                "expected a tool_use block, an array of blocks, or a message".into(),
            ));
        }
    };
    let mut out = Vec::new();
    for b in blocks {
        if b.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let name = b.get("name").and_then(Value::as_str).unwrap_or_default();
        let toolset = match b.get("toolset_name").and_then(Value::as_str) {
            Some("computer") => true,
            Some(_) => continue,
            None if name == "computer" => false,
            None => continue,
        };
        let id = b
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| bad(format!("tool_use block without an id: {b}")))?;
        let mut action: Map<String, Value> = match b.get("input") {
            Some(Value::Object(o)) => o.clone(),
            None | Some(Value::Null) => Map::new(),
            Some(_) => return Err(bad(format!("tool_use {id}: input must be an object"))),
        };
        let kind = if toolset {
            Value::from(name)
        } else {
            action
                .remove("action")
                .ok_or_else(|| bad(format!("tool_use {id}: input.action is missing")))?
        };
        action.insert("type".into(), kind);
        out.push(Call {
            id: id.to_string(),
            toolset,
            action: Value::Object(action),
        });
    }
    Ok(out)
}

/// The image limit to use when none is given: the toolset implies Claude Opus 4.7 or later.
pub fn default_limit(calls: &[Call]) -> ImageLimit {
    if calls.iter().any(|c| c.toolset) {
        ImageLimit::Large
    } else {
        ImageLimit::Standard
    }
}

/// Run the calls in order and return one `tool_result` block per call, plus whether all succeeded.
pub fn answer(runner: &mut Runner, calls: &[Call]) -> (Vec<Value>, bool) {
    let mut failed = false;
    let results = calls
        .iter()
        .map(|c| {
            let (content, is_error) = if failed {
                (vec![text(NOT_EXECUTED)], true)
            } else {
                match runner.run_one(&c.action) {
                    Ok(v) => (success(&c.action, v), false),
                    Err(e) => {
                        failed = true;
                        (
                            vec![text(&format!("Error: {} ({})", e.message, e.code))],
                            true,
                        )
                    }
                }
            };
            let mut r = json!({ "type": "tool_result", "tool_use_id": c.id });
            if c.toolset {
                r["toolset_name"] = json!("computer");
            }
            r["content"] = json!(content);
            if is_error {
                r["is_error"] = json!(true);
            }
            r
        })
        .collect();
    (results, !failed)
}

fn text(s: &str) -> Value {
    json!({ "type": "text", "text": s })
}

fn success(action: &Value, mut v: Value) -> Vec<Value> {
    if let Some(Value::String(b64)) = v.as_object_mut().and_then(|o| o.remove("png_base64")) {
        return vec![json!({
            "type": "image",
            "source": { "type": "base64", "media_type": "image/png", "data": b64 },
        })];
    }
    if action["type"] == "cursor_position" {
        let n = |k: &str| v[k].as_f64().unwrap_or(0.0).round() as i64;
        return vec![text(&format!("[{}, {}]", n("x"), n("y")))];
    }
    vec![text("OK")]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_shapes_are_recognized() {
        let msg = json!({ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "" },
            { "type": "tool_use", "id": "a", "name": "left_click", "toolset_name": "computer",
              "input": { "coordinate": [5, 6], "text": "shift" } },
            { "type": "tool_use", "id": "b", "name": "screenshot", "toolset_name": "computer",
              "input": {} },
            { "type": "tool_use", "id": "c", "name": "computer",
              "input": { "action": "key", "text": "ctrl+s" } },
            { "type": "tool_use", "id": "d", "name": "bash", "input": { "command": "ls" } },
            { "type": "tool_use", "id": "e", "name": "navigate", "toolset_name": "browser",
              "input": {} }
        ]});
        let c = calls(&msg).unwrap();
        assert_eq!(c.len(), 3);
        assert_eq!(
            c[0].action,
            json!({ "coordinate": [5, 6], "text": "shift", "type": "left_click" })
        );
        assert!(c[0].toolset && c[1].toolset && !c[2].toolset);
        assert_eq!(c[2].action, json!({ "text": "ctrl+s", "type": "key" }));
        assert_eq!(default_limit(&c), ImageLimit::Large);
        assert_eq!(default_limit(&c[2..]), ImageLimit::Standard);

        // A single block, and a legacy block without an action.
        assert_eq!(calls(&msg["content"][1]).unwrap().len(), 1);
        let bad = json!({ "type": "tool_use", "id": "x", "name": "computer", "input": {} });
        assert!(calls(&bad).is_err());
    }

    #[test]
    fn results_follow_the_api_shape() {
        let shot = success(
            &json!({"type": "zoom"}),
            json!({ "png_base64": "AAAA", "width": 1 }),
        );
        assert_eq!(shot[0]["source"]["data"], "AAAA");
        let pos = success(
            &json!({"type": "cursor_position"}),
            json!({ "x": 9.6, "y": 3.0 }),
        );
        assert_eq!(pos[0]["text"], "[10, 3]");
        assert_eq!(success(&json!({"type": "key"}), json!({}))[0]["text"], "OK");
    }
}
