//! `vway mcp` end to end: JSON-RPC over stdio, owned sessions, image content.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::time::Duration;

use common::*;
use serde_json::{Value, json};

struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn spawn(h: &Harness, extra: &[&str]) -> Mcp {
        let mut child = h
            .cmd()
            .arg("mcp")
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn vway mcp");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Mcp {
            child,
            stdin,
            stdout,
            next: 1,
        }
    }

    fn send(&mut self, v: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert_eq!(resp["id"], id);
        resp
    }

    fn tool(&mut self, name: &str, args: Value) -> (Value, Vec<Value>) {
        let resp = self.request("tools/call", json!({ "name": name, "arguments": args }));
        let result = &resp["result"];
        let content = result["content"].as_array().unwrap().clone();
        let text: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        let images = content
            .iter()
            .filter(|c| c["type"] == "image")
            .cloned()
            .collect();
        assert_eq!(
            result["isError"],
            text["ok"] == false,
            "isError mirrors ok: {text}"
        );
        (text, images)
    }
}

#[test]
fn mcp_drives_an_owned_session() {
    let h = Harness::new();
    let log = h.file("tc.log");
    let mut m = Mcp::spawn(&h, &[]);
    let init = m.request(
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "0" } }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "vway");
    m.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    let tools = m.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for t in [
        "start",
        "launch",
        "computer",
        "screenshot",
        "status",
        "stop",
        "list",
    ] {
        assert!(names.contains(&t), "{t} missing from {names:?}");
    }

    let (started, images) = m.tool(
        "start",
        json!({
            "command": [tc(), "--click-color", "00ff00"],
            "env": { "VWAY_TEST_LOG": log.to_str().unwrap() },
            "resolution": "320x240",
        }),
    );
    assert_eq!(started["ok"], true, "{started}");
    assert_eq!(started["window"]["ready"], true);
    assert_eq!(images.len(), 1, "start returns a screenshot");
    assert_eq!(images[0]["mimeType"], "image/png");
    let session = started["session"].as_str().unwrap().to_string();
    let sup = started["info"]["pid"].as_i64().unwrap();

    // A single Anthropic-style action, then a batch with a screenshot.
    let (r, _) = m.tool(
        "computer",
        json!({ "action": "left_click", "coordinate": [100, 100] }),
    );
    assert_eq!(r["ok"], true, "{r}");
    wait_events(&log, "click", |ev| !of(ev, "button").is_empty());
    let (r, images) = m.tool(
        "computer",
        json!({ "actions": [{ "type": "type", "text": "ok" }, { "type": "screenshot" }] }),
    );
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(images.len(), 1);
    assert_eq!(
        r["results"][1]["result"]["image"], 1,
        "results point at their image block"
    );
    assert!(r["results"][1]["result"].get("png_base64").is_none());

    // Failures come back as tool errors, not JSON-RPC errors.
    let (r, _) = m.tool("computer", json!({ "action": "key", "text": "NoSuchKey" }));
    assert_eq!(r["results"][0]["error"]["code"], "invalid_key");
    let (r, images) = m.tool(
        "computer",
        json!({ "actions": [
            { "type": "zoom", "region": [10, 20, 50, 40] },
            { "type": "key", "text": "NoSuchKey" },
            { "type": "screenshot" }
        ] }),
    );
    assert_eq!(images.len(), 1);
    assert_eq!(
        r["results"][0]["result"]["width"], 40,
        "zoom regions are corners"
    );
    assert_eq!(r["results"][0]["result"]["height"], 20);
    assert_eq!(
        r["results"][2]["error"]["message"],
        "Not executed: an earlier computer action in this turn failed."
    );

    let (st, _) = m.tool("status", json!({}));
    assert_eq!(st["windows"].as_array().unwrap().len(), 1);
    let (list, _) = m.tool("list", json!({}));
    assert_eq!(list["sessions"][0]["owned"], true);
    let (shot, images) = m.tool(
        "screenshot",
        json!({ "session": session, "region": [0, 0, 20, 20] }),
    );
    assert_eq!(shot["width"], 20);
    assert_eq!(images.len(), 1);

    // EOF on stdin stops owned sessions.
    drop(m.stdin.take());
    let status = m.child.wait().unwrap();
    assert!(status.success());
    wait_until("owned session stop", Duration::from_secs(5), || {
        !pid_alive(sup)
    });
}

#[test]
fn mcp_allow_list() {
    let h = Harness::new();
    let tc = tc();
    let dir = format!("{}/", testclient().parent().unwrap().display());
    let mut m = Mcp::spawn(&h, &["--allow", &dir]);
    m.request("initialize", json!({ "protocolVersion": "2025-06-18" }));
    let (r, _) = m.tool("start", json!({ "command": ["sh", "-c", "true"] }));
    assert_eq!(r["error"]["code"], "not_allowed", "{r}");
    let (r, _) = m.tool("start", json!({ "command": [tc], "screenshot": false }));
    assert_eq!(r["ok"], true, "{r}");
    let (r, _) = m.tool("stop", json!({}));
    assert_eq!(r["ok"], true);
    assert_eq!(r["exit"]["reason"], "shutdown_request");
    let (r, _) = m.tool("status", json!({}));
    assert_eq!(r["error"]["code"], "session_not_found");
}
