//! End-to-end tests: real supervisors, the real CLI, and the shm test client.

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};

const RED: u32 = 0xff0000;
const GREEN: u32 = 0x00ff00;
const WHITE: u32 = 0xffffff;

fn env_arg(log: &std::path::Path) -> String {
    format!("VWAY_TEST_LOG={}", log.display())
}

fn batch_pixels_and_input(renderer: Option<&str>) {
    let h = Harness::new();
    let log = h.file("tc.log");
    let (a, b) = (h.file("a.png"), h.file("b.png"));
    let tc = tc();
    let mut cmd = h.cmd();
    if let Some(r) = renderer {
        cmd.env("VWAY_RENDERER", r);
    }
    cmd.args(["run", "--resolution", "400x300", "--env", &env_arg(&log)])
        .args(["--screenshot", a.to_str().unwrap()])
        .args(["--click", "200,150"])
        .args(["--screenshot", b.to_str().unwrap()])
        .args(["--", &tc, "--click-color", "00ff00"]);
    let out = h.run_with(&mut cmd);
    assert_eq!(out.code, 0, "{}\n{}", out.json, out.stderr);
    assert_eq!(out.json["window"]["ready"], true);
    let types: Vec<&str> = out.json["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        ["screenshot", "left_click", "screenshot"],
        "inline actions keep CLI order"
    );

    let first = Image::load(&a);
    assert_eq!((first.width, first.height), (400, 300));
    first.assert_rgb(2, 2, WHITE); // marker: top-left, so flips are caught
    first.assert_rgb(15, 15, RED);
    first.assert_rgb(200, 150, RED);
    first.assert_rgb(399, 299, RED);
    // The screenshot after the click settles until the client has redrawn.
    let second = Image::load(&b);
    second.assert_rgb(200, 150, GREEN);

    let ev = events(&log);
    let buttons = of(&ev, "button");
    assert_eq!(buttons.len(), 2, "press + release: {buttons:?}");
    assert_eq!(buttons[0]["pressed"], true);
    assert_eq!(buttons[0]["button"], 0x110);
    assert_eq!(
        (buttons[0]["x"].as_f64(), buttons[0]["y"].as_f64()),
        (Some(200.0), Some(150.0))
    );
    // The owned session is cleaned up after a successful run.
    let sessions = std::fs::read_dir(h.root.join("sessions")).unwrap().count();
    assert_eq!(sessions, 0, "run removes its session directory on success");
}

#[test]
fn batch_run_pixels_and_input_gles_or_default() {
    batch_pixels_and_input(None);
}

#[test]
fn batch_run_pixels_and_input_pixman() {
    batch_pixels_and_input(Some("pixman"));
}

#[test]
fn lifecycle_start_status_stop() {
    let h = Harness::new();
    let started = h.start_tc("life", &[], &[]);
    assert_eq!(started["session"], "life");
    let sup_pid = started["info"]["pid"].as_i64().unwrap();
    let app_pid = started["launch"]["pid"].as_i64().unwrap();
    assert!(started["window"]["ready"] == true);
    assert!(["gles", "pixman"].contains(&started["info"]["renderer"].as_str().unwrap()));

    let st = h.ok(&["status", "-s", "life"]);
    assert_eq!(st["windows"].as_array().unwrap().len(), 1);
    assert_eq!(st["windows"][0]["focused"], true);
    assert_eq!(st["windows"][0]["app_id"], "dev.vway.testclient");
    assert_eq!(st["app"]["running"], true);
    assert_eq!(st["focus"], st["windows"][0]["id"]);

    // The only running session is the default.
    let st2 = h.ok(&["status"]);
    assert_eq!(st2["session"], "life");

    let list = h.ok(&["list"]);
    assert_eq!(list["sessions"][0]["alive"], true);

    let stopped = h.ok(&["stop", "-s", "life"]);
    assert_eq!(stopped["exit"]["reason"], "shutdown_request");
    assert!(!pid_alive(sup_pid), "supervisor gone");
    wait_until("app exit", Duration::from_secs(2), || !pid_alive(app_pid));
    let dir = h.session_dir("life");
    assert!(!dir.join("ctl.sock").exists());
    assert!(!dir.join("home").exists(), "isolated home is deleted");

    let list = h.ok(&["list", "--all"]);
    assert_eq!(list["sessions"][0]["alive"], false);
    assert_eq!(list["sessions"][0]["exit"]["reason"], "shutdown_request");
    h.fails(&["status", "-s", "life"], "session_not_found");
    let gc = h.ok(&["gc"]);
    assert_eq!(gc["removed"], json!(["life"]));
    assert!(!dir.exists());
}

#[test]
fn signals_tear_everything_down() {
    let h = Harness::new();
    for (id, sig) in [("term", libc::SIGTERM), ("kill", libc::SIGKILL)] {
        let started = h.start_tc(id, &[], &[]);
        let sup = started["info"]["pid"].as_i64().unwrap();
        let app = started["launch"]["pid"].as_i64().unwrap();
        unsafe { libc::kill(sup as i32, sig) };
        wait_until("supervisor exit", Duration::from_secs(5), || {
            !pid_alive(sup)
        });
        // SIGKILL: the app's pdeathsig takes it down; SIGTERM: the supervisor does.
        wait_until("app exit", Duration::from_secs(3), || !pid_alive(app));
        let list = h.ok(&["list", "--all"]);
        let entry = list["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == id)
            .unwrap()
            .clone();
        assert_eq!(entry["alive"], false, "{id}: {entry}");
        if sig == libc::SIGTERM {
            assert_eq!(entry["exit"]["reason"], "signal_term", "{entry}");
            assert!(!h.session_dir(id).join("ctl.sock").exists());
        }
        // A dead session's id can be reused without --replace.
        h.ok(&["start", "--id", id]);
        h.ok(&["stop", "-s", id]);
    }
}

#[test]
fn duplicate_id_and_replace() {
    let h = Harness::new();
    let first = h.ok(&["start", "--id", "dup"]);
    h.fails(&["start", "--id", "dup"], "session_exists");
    let second = h.ok(&["start", "--id", "dup", "--replace"]);
    assert_ne!(first["info"]["pid"], second["info"]["pid"]);
    assert!(!pid_alive(first["info"]["pid"].as_i64().unwrap()));
    h.fails(&["start", "--id", "../evil"], "usage");
}

#[test]
fn owned_session_dies_with_the_cli() {
    let h = Harness::new();
    let tc = tc();
    let mut child = h
        .cmd()
        .args(["run", "--until-exit", "--", &tc])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let sessions = h.root.join("sessions");
    let mut info = Value::Null;
    wait_until("owned session ready", Duration::from_secs(10), || {
        std::fs::read_dir(&sessions)
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.path().join("info.json").exists())
    });
    for e in std::fs::read_dir(&sessions).unwrap().flatten() {
        if let Ok(s) = std::fs::read_to_string(e.path().join("info.json")) {
            info = serde_json::from_str(&s).unwrap();
        }
    }
    let sup = info["pid"].as_i64().unwrap();
    // Wait for the app so we can check that it goes too.
    let st = h.ok(&["wait", "window"]);
    assert_eq!(st["window"]["ready"], true);
    let app = h.ok(&["status"])["app"]["pid"].as_i64().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    wait_until("owned supervisor exit", Duration::from_secs(5), || {
        !pid_alive(sup)
    });
    wait_until("app exit", Duration::from_secs(3), || !pid_alive(app));
}

#[test]
fn window_wait_needs_a_buffer_not_just_a_toplevel() {
    let h = Harness::new();
    h.ok(&["start", "--id", "late"]);
    let tc = tc();
    let t0 = Instant::now();
    h.ok(&[
        "launch",
        "-s",
        "late",
        "--no-wait",
        "--",
        &tc,
        "--first-frame-delay-ms",
        "700",
    ]);
    std::thread::sleep(Duration::from_millis(200));
    let st = h.ok(&["status", "-s", "late"]);
    let windows = st["windows"].as_array().unwrap();
    assert!(
        windows.iter().all(|w| w["ready"] == false),
        "a toplevel without a buffer is not ready: {st}"
    );
    let w = h.ok(&["wait", "-s", "late", "window"]);
    assert!(
        t0.elapsed() >= Duration::from_millis(650),
        "returned before the first frame"
    );
    assert_eq!(w["window"]["ready"], true);
    let shot = h.file("late.png");
    h.ok(&["screenshot", "-s", "late", "-o", shot.to_str().unwrap()]);
    Image::load(&shot).assert_rgb(100, 100, RED);
}

#[test]
fn typing_keys_and_unmappable_text() {
    let h = Harness::new();
    let log = h.file("tc.log");
    h.start_tc("keys", &["--env", &env_arg(&log)], &[]);
    h.ok(&["type", "-s", "keys", "Hi there, 42!"]);
    // Every press has a matching release (the last release can arrive after the last press).
    wait_events(&log, "typed text, all keys released", |ev| {
        let keys = of(ev, "key");
        let presses: Vec<_> = keys.iter().filter(|k| k["pressed"] == true).collect();
        let typed: String = presses.iter().filter_map(|k| k["utf8"].as_str()).collect();
        typed == "Hi there, 42!" && presses.len() * 2 == keys.len()
    });

    // Unmappable text fails before typing anything.
    let before = events(&log).len();
    let out = h.fails(&["type", "-s", "keys", "aö"], "unmappable_text");
    assert!(out["error"]["message"].as_str().unwrap().contains('ö'));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(events(&log).len(), before, "nothing typed");

    h.ok(&["key", "-s", "keys", "ctrl+shift+e"]);
    wait_events(&log, "ctrl+shift+e", |ev| {
        of(ev, "key").iter().any(|k| {
            k["sym"] == "E" && k["pressed"] == true && k["mods"] == json!(["ctrl", "shift"])
        })
    });
    h.fails(&["key", "-s", "keys", "ctrl+NoSuchKey"], "invalid_key");
    // Modifiers are released after a combo: plain typing is unmodified.
    h.ok(&["type", "-s", "keys", "x"]);
    wait_events(&log, "plain x", |ev| {
        of(ev, "key")
            .iter()
            .any(|k| k["utf8"] == "x" && k["mods"] == json!([]))
    });
}

#[test]
fn touch_tap_and_pinch() {
    let h = Harness::new();
    let log = h.file("tc.log");
    h.start_tc(
        "touch",
        &["--env", &env_arg(&log)],
        &["--click-color", "00ff00"],
    );
    h.ok(&["touch", "-s", "touch", "tap", "100,120"]);
    let ev = wait_events(&log, "tap", |ev| !of(ev, "touch_up").is_empty());
    let down = of(&ev, "touch_down")[0];
    assert_eq!(
        (down["x"].as_f64(), down["y"].as_f64()),
        (Some(100.0), Some(120.0))
    );
    let shot = h.file("t.png");
    h.ok(&["screenshot", "-s", "touch", "-o", shot.to_str().unwrap()]);
    Image::load(&shot).assert_rgb(300, 300, GREEN);

    h.ok(&[
        "touch",
        "-s",
        "touch",
        "pinch",
        "300,300",
        "--from",
        "200",
        "--to",
        "100",
        "--duration-ms",
        "100",
    ]);
    let ev = wait_events(&log, "pinch", |ev| of(ev, "touch_up").len() >= 3);
    let ids: std::collections::BTreeSet<i64> = of(&ev, "touch_down")
        .iter()
        .skip(1)
        .map(|d| d["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "two fingers: {ids:?}");
    assert!(!of(&ev, "touch_motion").is_empty());

    // Invalid sequences are rejected up front.
    let bad = r#"[{"type":"vway_touch","events":[{"phase":"up","id":7}]}]"#;
    let script = h.file("bad.json");
    std::fs::write(&script, bad).unwrap();
    let out = h.run(&["do", "-s", "touch", script.to_str().unwrap()]);
    assert_eq!(out.code, 1);
    assert_eq!(out.json["results"][0]["error"]["code"], "bad_request");
}

#[test]
fn scroll_reports_wheel_clicks() {
    let h = Harness::new();
    let log = h.file("tc.log");
    h.start_tc("scroll", &["--env", &env_arg(&log)], &[]);
    h.ok(&["scroll", "-s", "scroll", "down", "2", "--at", "50,60"]);
    h.ok(&["scroll", "-s", "scroll", "left", "1"]);
    let ev = wait_events(&log, "scroll", |ev| of(ev, "axis120").len() >= 2);
    let a = of(&ev, "axis120");
    assert_eq!(a[0]["axis"], "vertical");
    assert_eq!(a[0]["value120"], 240);
    assert_eq!(a[1]["axis"], "horizontal");
    assert_eq!(a[1]["value120"], -120);
    let st = h.ok(&["status", "-s", "scroll"]);
    assert_eq!(st["pointer"], json!({"x": 50.0, "y": 60.0}));
}

#[test]
fn resize_and_fractional_scale() {
    let h = Harness::new();
    let log = h.file("tc.log");
    h.start_tc(
        "rs",
        &["--env", &env_arg(&log), "--resolution", "400x300"],
        &[],
    );
    let r = h.ok(&["resize", "-s", "rs", "500x400", "--scale", "1.5"]);
    assert_eq!(r["physical_width"], 750);
    wait_events(&log, "reconfigure", |ev| {
        of(ev, "draw")
            .iter()
            .any(|d| d["width"] == 500 && d["buffer_width"] == 750)
    });
    let shot = h.file("s.png");
    let s = h.ok(&["screenshot", "-s", "rs", "-o", shot.to_str().unwrap()]);
    assert_eq!(
        (s["width"].as_u64(), s["height"].as_u64()),
        (Some(750), Some(600))
    );
    let img = Image::load(&shot);
    img.assert_rgb(13, 13, WHITE); // 10 logical px marker = 15 physical
    img.assert_rgb(16, 16, RED);
    img.assert_rgb(749, 599, RED);

    // Regions are logical; the result is physical.
    let crop = h.file("c.png");
    let c = h.ok(&[
        "screenshot",
        "-s",
        "rs",
        "-o",
        crop.to_str().unwrap(),
        "--region",
        "0,0,20,10",
    ]);
    assert_eq!(
        (c["width"].as_u64(), c["height"].as_u64()),
        (Some(30), Some(15))
    );
    let ci = Image::load(&crop);
    ci.assert_rgb(1, 1, WHITE);
    ci.assert_rgb(25, 5, RED);
    h.fails(
        &["screenshot", "-s", "rs", "--region", "900,900,10,10"],
        "out_of_range",
    );
    h.fails(
        &["resize", "-s", "rs", "500x400", "--scale", "1.33"],
        "out_of_range",
    ); // not k/120
}

#[test]
fn environment_is_isolated() {
    let h = Harness::new();
    let log = h.file("tc.log");
    let started = h.start_tc(
        "iso",
        &["--env", &env_arg(&log), "--env", "VWAY_TEST_MARK=1"],
        &["--env-log"],
    );
    let dir = h.session_dir("iso");
    let ev = wait_events(&log, "env", |ev| !of(ev, "env").is_empty());
    let vars = &of(&ev, "env")[0]["vars"];
    assert_eq!(vars["HOME"], dir.join("home").to_str().unwrap());
    assert_eq!(vars["XDG_RUNTIME_DIR"], dir.join("xdg").to_str().unwrap());
    assert_eq!(vars["WAYLAND_DISPLAY"], "wayland-0");
    assert_eq!(vars["DISPLAY"], Value::Null, "no X11 leak");
    assert_eq!(
        vars["XDG_CONFIG_HOME"],
        dir.join("home/.config").to_str().unwrap()
    );
    assert_eq!(vars["VWAY_TEST_MARK"], "1", "session --env applies");
    assert_eq!(vars["VWAY_SESSION"], "iso");
    let bus = vars["DBUS_SESSION_BUS_ADDRESS"].as_str().unwrap_or("");
    let has_dbus = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .any(|d| d.join("dbus-daemon").is_file());
    if has_dbus {
        assert!(bus.contains(dir.to_str().unwrap()), "private bus: {bus}");
    }
    assert!(started["launch"]["warnings"].is_array());

    // Per-launch env: K= unsets, cwd is the caller's.
    h.ok(&["close-app", "-s", "iso"]);
    let tc = tc();
    let cwd = h.root.clone();
    let out = h.run_with(h.cmd().current_dir(&cwd).args([
        "launch",
        "-s",
        "iso",
        "--env",
        "VWAY_TEST_MARK=",
        "--",
        &tc,
        "--env-log",
    ]));
    assert_eq!(out.code, 0, "{}", out.json);
    let ev = wait_events(&log, "second env", |ev| of(ev, "env").len() >= 2);
    let second = of(&ev, "env")[1];
    assert_eq!(second["vars"]["VWAY_TEST_MARK"], Value::Null);
    assert_eq!(second["cwd"], cwd.to_str().unwrap());

    h.ok(&["stop", "-s", "iso"]);
    assert!(!dir.join("home").exists());
}

#[test]
fn multiple_windows_and_focus() {
    let h = Harness::new();
    let log = h.file("tc.log");
    h.start_tc(
        "multi",
        &["--env", &env_arg(&log), "--window-mode", "floating"],
        &["--windows", "2"],
    );
    h.ok(&[
        "wait",
        "-s",
        "multi",
        "window",
        "--title",
        "vway-testclient-1",
    ]);
    let st = h.ok(&["status", "-s", "multi"]);
    let windows = st["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 2);
    let first = windows
        .iter()
        .find(|w| w["title"] == "vway-testclient")
        .unwrap();
    let id = first["id"].as_u64().unwrap();
    h.ok(&["focus", "-s", "multi", &id.to_string()]);
    let st = h.ok(&["status", "-s", "multi"]);
    assert_eq!(st["focus"], id);
    wait_events(&log, "focus on window 0", |ev| {
        of(ev, "focus")
            .last()
            .is_some_and(|f| f["window"] == 0 && f["focused"] == true)
    });
    h.fails(&["focus", "-s", "multi", "999"], "no_window");

    // close-window asks politely; the client exits once its last window is gone.
    for w in st["windows"].as_array().unwrap() {
        h.ok(&["close-window", "-s", "multi", &w["id"].to_string()]);
    }
    let exit = h.ok(&["wait", "-s", "multi", "exit"]);
    assert!(exit["waited_ms"].is_u64());
    let st = h.ok(&["status", "-s", "multi"]);
    assert_eq!(st["app"]["running"], false);
    assert_eq!(st["app"]["exit_code"], 0);
}

#[test]
fn app_exit_is_reported() {
    let h = Harness::new();
    h.ok(&["start", "--id", "exit"]);
    let tc = tc();
    h.ok(&[
        "launch",
        "-s",
        "exit",
        "--",
        &tc,
        "--duration",
        "300",
        "--exit-code",
        "3",
    ]);
    h.fails(&["launch", "-s", "exit", "--", &tc], "app_running");
    h.ok(&["wait", "-s", "exit", "exit", "--timeout-ms", "5000"]);
    let st = h.ok(&["status", "-s", "exit"]);
    assert_eq!(st["app"]["exit_code"], 3);

    // An app that exits without a window fails the window wait immediately.
    let t0 = Instant::now();
    let out = h.fails(&["launch", "-s", "exit", "--", "false"], "app_exited");
    assert!(t0.elapsed() < Duration::from_secs(3), "{out}");
    h.fails(
        &["launch", "-s", "exit", "--", "/nonexistent/app"],
        "launch_failed",
    );

    // --exit-with-app ends the session with the app.
    let started = h.start_tc("ewa", &["--exit-with-app"], &["--duration", "200"]);
    let sup = started["info"]["pid"].as_i64().unwrap();
    wait_until("session exit", Duration::from_secs(5), || !pid_alive(sup));
    let exit: Value = serde_json::from_str(
        &std::fs::read_to_string(h.session_dir("ewa").join("exit.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(exit["reason"], "app_exited");
}

#[test]
fn scripts_stop_at_the_first_failure() {
    let h = Harness::new();
    h.start_tc("script", &[], &[]);
    let script = h.file("s.json");
    std::fs::write(
        &script,
        r#"{"vway_action": 1, "actions": [
            {"type": "left_click", "coordinate": [10, 10]},
            {"type": "key", "text": "NoSuchKey"},
            {"type": "type", "text": "never"}
        ]}"#,
    )
    .unwrap();
    let out = h.run(&["do", "-s", "script", script.to_str().unwrap()]);
    assert_eq!(out.code, 1);
    assert_eq!(out.json["ok"], false);
    let r = out.json["results"].as_array().unwrap();
    assert_eq!(r[0]["ok"], true);
    assert_eq!(r[1]["error"]["code"], "invalid_key");
    assert_eq!(r[2]["error"]["code"], "not_executed");
    assert!(out.stderr.contains("action 1"), "{}", out.stderr);

    // Scripts from stdin, screenshots into --out-dir, `action` as an alias of `type`.
    let shots = h.file("shots");
    let mut child = h
        .cmd()
        .args([
            "do",
            "-s",
            "script",
            "-",
            "--out-dir",
            shots.to_str().unwrap(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"[{"action": "screenshot"}, {"type": "cursor_position"}, {"type": "zoom", "region": [0, 0, 50, 50]}]"#)
        .unwrap();
    let o = child.wait_with_output().unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["results"][1]["result"], json!({"x": 10.0, "y": 10.0}));
    assert!(shots.join("shot-001.png").exists());
    assert_eq!(v["results"][2]["result"]["width"], 50);

    // Unknown fields are script errors, not silently ignored.
    std::fs::write(
        &script,
        r#"[{"type": "left_click", "coordinates": [1, 1]}]"#,
    )
    .unwrap();
    let out = h.run(&["do", "-s", "script", script.to_str().unwrap()]);
    assert_eq!(out.json["results"][0]["error"]["code"], "script_error");
}

#[test]
fn usage_errors_are_json_with_exit_2() {
    let h = Harness::new();
    let out = h.run(&["click", "--bogus"]);
    assert_eq!(out.code, 2);
    assert_eq!(out.json["error"]["code"], "usage");
    let out = h.run(&["start", "--resolution", "0x10"]);
    assert_eq!(out.code, 2);
    let out = h.run(&["status"]);
    assert_eq!(out.code, 1);
    assert_eq!(out.json["error"]["code"], "session_not_found");
    let help = Command::new(vway_bin()).arg("--help").output().unwrap();
    assert!(help.status.success());
}

#[test]
fn drag_and_mouse_buttons() {
    let h = Harness::new();
    let log = h.file("tc.log");
    h.start_tc("drag", &["--env", &env_arg(&log)], &[]);
    h.ok(&[
        "drag",
        "-s",
        "drag",
        "10,10",
        "110,60",
        "--duration-ms",
        "100",
    ]);
    let ev = wait_events(&log, "drag", |ev| of(ev, "button").len() >= 2);
    let motions = of(&ev, "motion");
    assert!(motions.len() >= 3, "interpolated motion: {motions:?}");
    let last = motions.last().unwrap();
    assert_eq!(
        (last["x"].as_f64(), last["y"].as_f64()),
        (Some(110.0), Some(60.0))
    );
    h.ok(&["mouse", "-s", "drag", "down", "--button", "right"]);
    h.ok(&["mouse", "-s", "drag", "up", "--button", "right"]);
    h.ok(&[
        "click", "-s", "drag", "20,20", "--count", "2", "--mods", "shift",
    ]);
    let ev = wait_events(&log, "right + double click", |ev| {
        of(ev, "button").len() >= 8
    });
    let b = of(&ev, "button");
    assert_eq!(b[2]["button"], 0x111);
    let shift_held = of(&ev, "key")
        .iter()
        .any(|k| k["sym"] == "Shift_L" && k["pressed"] == true);
    assert!(shift_held, "click --mods holds shift");
}

/// Run `vway ARGS` with `input` on stdin; returns the exit code and the JSON document.
fn run_stdin(h: &Harness, args: &[&str], input: &str) -> (i32, Value) {
    use std::io::Write;
    let mut child = h
        .cmd()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|e| {
        panic!(
            "{e}: {}\n{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    });
    (o.status.code().unwrap_or(-1), v)
}

#[test]
fn action_coordinates_follow_the_screenshots() {
    let h = Harness::new();
    let log = h.file("tc.log");
    h.start_tc(
        "big",
        &["--env", &env_arg(&log), "--resolution", "1920x1080"],
        &[],
    );
    // standard: 1920x1080 does not fit 1568 px / 1.15 MP, so screenshots shrink and coordinates
    // are pixels of the shrunk image.
    let shot = h.file("big.png");
    let script = json!([
        { "type": "screenshot", "path": shot },
        { "type": "left_click", "coordinate": [714, 402] },
        { "type": "cursor_position" },
        { "type": "zoom", "region": [0, 0, 100, 50] }
    ]);
    let (code, v) = run_stdin(
        &h,
        &["do", "-s", "big", "-", "--image-limit", "standard"],
        &script.to_string(),
    );
    assert_eq!(code, 0, "{v}");
    let r = &v["results"];
    assert_eq!(
        (&r[0]["result"]["width"], &r[0]["result"]["height"]),
        (&json!(1429), &json!(804))
    );
    let img = Image::load(&shot);
    assert_eq!((img.width, img.height), (1429, 804));
    img.assert_rgb(3, 3, WHITE); // the 10 px marker, shrunk to ~7 px
    img.assert_rgb(12, 12, RED);
    let ev = wait_events(&log, "click", |ev| !of(ev, "button").is_empty());
    let b = of(&ev, "button")[0];
    let (x, y) = (b["x"].as_f64().unwrap(), b["y"].as_f64().unwrap());
    assert!((x - 960.0).abs() <= 1.0 && (y - 540.0).abs() <= 1.0, "{b}");
    assert_eq!(r[2]["result"], json!({ "x": 714.0, "y": 402.0 }));
    // A zoom returns its region at full resolution: 100x50 screenshot px = 134x67 output px.
    assert_eq!(
        (&r[3]["result"]["width"], &r[3]["result"]["height"]),
        (&json!(134), &json!(67))
    );

    // Without a limit (the CLI default) screenshots and coordinates are logical pixels.
    let (code, v) = run_stdin(&h, &["do", "-s", "big", "-"], r#"[{"type": "screenshot"}]"#);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["results"][0]["result"]["width"], 1920);
}

#[test]
fn hidpi_actions_use_logical_screenshots_and_full_resolution_zoom() {
    let h = Harness::new();
    let log = h.file("tc.log");
    h.start_tc(
        "hi",
        &[
            "--env",
            &env_arg(&log),
            "--resolution",
            "400x300",
            "--scale",
            "2",
        ],
        &[],
    );
    let shot = h.file("hi.png");
    let script = json!([
        { "type": "screenshot", "path": shot },
        { "type": "left_click", "coordinate": [200, 150] },
        { "type": "zoom", "region": [0, 0, 20, 10] }
    ]);
    let (code, v) = run_stdin(&h, &["do", "-s", "hi", "-"], &script.to_string());
    assert_eq!(code, 0, "{v}");
    let img = Image::load(&shot);
    assert_eq!(
        (img.width, img.height),
        (400, 300),
        "action screenshots are logical size"
    );
    img.assert_rgb(4, 4, WHITE);
    img.assert_rgb(12, 12, RED);
    let ev = wait_events(&log, "click", |ev| !of(ev, "button").is_empty());
    let b = of(&ev, "button")[0];
    assert_eq!(
        (b["x"].as_f64(), b["y"].as_f64()),
        (Some(200.0), Some(150.0))
    );
    let z = &v["results"][2]["result"];
    assert_eq!(
        (&z["width"], &z["height"]),
        (&json!(40), &json!(20)),
        "zoom is physical"
    );
    // The low-level command still captures physical pixels.
    let s = h.ok(&[
        "screenshot",
        "-s",
        "hi",
        "-o",
        h.file("raw.png").to_str().unwrap(),
    ]);
    assert_eq!(s["width"], 800);
}

#[test]
fn computer_answers_tool_use_blocks() {
    let h = Harness::new();
    let log = h.file("tc.log");
    h.start_tc("pt", &["--env", &env_arg(&log)], &[]);
    // A computer_toolset_20260801 response, passed through as it came from the API.
    let response = json!({ "role": "assistant", "content": [
        { "type": "text", "text": "Dragging." },
        { "type": "tool_use", "id": "toolu_1", "name": "left_click_drag", "toolset_name": "computer",
          "input": { "start_coordinate": [10, 10], "coordinate": [60, 40], "text": "shift" } },
        { "type": "tool_use", "id": "toolu_2", "name": "key", "toolset_name": "computer",
          "input": { "text": "ctrl+a BackSpace" } },
        { "type": "tool_use", "id": "toolu_3", "name": "cursor_position", "toolset_name": "computer",
          "input": {} },
        { "type": "tool_use", "id": "toolu_4", "name": "screenshot", "toolset_name": "computer",
          "input": {} }
    ]});
    let (code, v) = run_stdin(&h, &["computer", "-s", "pt"], &response.to_string());
    assert_eq!(code, 0, "{v}");
    let r = v["tool_results"].as_array().unwrap();
    assert_eq!(r.len(), 4);
    for (i, res) in r.iter().enumerate() {
        assert_eq!(res["type"], "tool_result");
        assert_eq!(res["tool_use_id"], format!("toolu_{}", i + 1));
        assert_eq!(res["toolset_name"], "computer");
        assert!(res.get("is_error").is_none(), "{res}");
    }
    assert_eq!(r[0]["content"], json!([{ "type": "text", "text": "OK" }]));
    assert_eq!(r[2]["content"][0]["text"], "[60, 40]");
    let img = &r[3]["content"][0];
    assert_eq!(img["type"], "image");
    assert_eq!(img["source"]["media_type"], "image/png");
    assert!(img["source"]["data"].as_str().unwrap().len() > 100);

    let ev = wait_events(&log, "drag and keys", |ev| {
        of(ev, "key").iter().any(|k| k["sym"] == "BackSpace")
    });
    let keys = of(&ev, "key");
    let pressed = |sym: &str| keys.iter().any(|k| k["sym"] == sym && k["pressed"] == true);
    assert!(pressed("Shift_L"), "the drag holds shift");
    assert!(
        pressed("a") && pressed("BackSpace"),
        "key sequences press each key: {keys:?}"
    );
    assert_eq!(of(&ev, "button").len(), 2);

    // computer_20251124 blocks: results carry no toolset_name; after a failure the rest get
    // Anthropic's halt text.
    let blocks = json!([
        { "type": "tool_use", "id": "a", "name": "computer",
          "input": { "action": "key", "text": "NoSuchKey" } },
        { "type": "tool_use", "id": "b", "name": "computer", "input": { "action": "screenshot" } }
    ]);
    let (code, v) = run_stdin(&h, &["computer", "-s", "pt", "-"], &blocks.to_string());
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    let r = &v["tool_results"];
    assert!(r[0].get("toolset_name").is_none());
    assert_eq!(r[0]["is_error"], true);
    assert!(
        r[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Error: "),
        "{}",
        r[0]
    );
    assert_eq!(
        r[1]["content"][0]["text"],
        "Not executed: an earlier computer action in this turn failed."
    );
}
