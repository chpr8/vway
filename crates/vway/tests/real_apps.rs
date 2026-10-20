//! Real applications on the host: `cargo test -p vway --test real_apps -- --ignored`.
//!
//! Ignored by default because they depend on installed programs and the GPU stack. A missing
//! program skips its test (with a note on stderr) rather than failing it.

mod common;

use common::*;

fn require(prog: &str) -> bool {
    if which(prog).is_some() {
        return true;
    }
    eprintln!("skipping: {prog} is not installed");
    false
}

/// Start `argv` in session `id`, type a command line, and check the window changed.
fn terminal_echoes(h: &Harness, id: &str, extra: &[&str], argv: &[&str]) -> serde_json::Value {
    let mut args = vec!["start", "--id", id, "--resolution", "640x400"];
    args.extend_from_slice(extra);
    args.push("--");
    args.extend_from_slice(argv);
    let started = h.ok(&args);
    assert_eq!(started["window"]["ready"], true, "{started}");

    let (a, b) = (
        h.file(&format!("{id}-a.png")),
        h.file(&format!("{id}-b.png")),
    );
    h.ok(&["screenshot", "-s", id, "-o", a.to_str().unwrap()]);
    h.ok(&["type", "-s", id, "echo vway-$((6*7))"]);
    h.ok(&["key", "-s", id, "Return"]);
    h.ok(&["screenshot", "-s", id, "-o", b.to_str().unwrap()]);
    let (a, b) = (Image::load(&a), Image::load(&b));
    assert_eq!((a.width, a.height), (640, 400));
    assert!(a.colors() > 1, "{id}: the first frame is blank");
    assert!(b.diff(&a) > 200, "{id}: typing did not change the window");

    let st = h.ok(&["status", "-s", id]);
    assert_eq!(st["windows"].as_array().unwrap().len(), 1, "{st}");
    assert!(st.get("client_errors").is_none(), "{st}");
    started
}

#[test]
#[ignore = "needs foot"]
fn foot_shm_terminal() {
    if !require("foot") {
        return;
    }
    let h = Harness::new();
    // foot reads no user config here: the session has an isolated HOME.
    terminal_echoes(&h, "foot", &[], &["foot"]);
}

#[test]
#[ignore = "needs alacritty and a working EGL stack"]
fn alacritty_egl_terminal() {
    if !require("alacritty") {
        return;
    }
    let h = Harness::new();
    terminal_echoes(&h, "ala", &[], &["alacritty"]);
    // Software GL (llvmpipe) through the same protocol path.
    terminal_echoes(&h, "ala-sw", &["--gpu", "software"], &["alacritty"]);
}

#[test]
#[ignore = "needs vkcube and a Vulkan driver"]
fn vkcube_vulkan_wsi() {
    if !require("vkcube") {
        return;
    }
    let h = Harness::new();
    let started = h.ok(&[
        "start",
        "--id",
        "vk",
        "--resolution",
        "640x480",
        "--scale",
        "1.5",
        "--",
        "vkcube",
        "--wsi",
        "wayland",
    ]);
    let renderer = started["info"]["renderer"].as_str().unwrap().to_string();
    eprintln!("renderer: {renderer}");
    let shot = h.file("vk.png");
    // vkcube never goes idle; take the frame as is.
    h.ok(&[
        "screenshot",
        "-s",
        "vk",
        "--no-settle",
        "-o",
        shot.to_str().unwrap(),
    ]);
    let img = Image::load(&shot);
    assert_eq!((img.width, img.height), (960, 720));
    assert!(img.colors() > 16, "the cube is not visible");
    let st = h.ok(&["status", "-s", "vk"]);
    assert!(st.get("client_errors").is_none(), "{st}");
}

/// Under the CPU-only renderer, Vulkan buffers the CPU cannot map (RADV puts linear swapchain
/// images in VRAM) must fail fast and visibly, not hang until the window timeout.
#[test]
#[ignore = "needs vkcube and a Vulkan driver"]
fn vkcube_under_pixman_fails_fast_or_works() {
    if !require("vkcube") {
        return;
    }
    let h = Harness::new();
    let mut cmd = h.cmd();
    cmd.env("VWAY_RENDERER", "pixman").args([
        "start",
        "--id",
        "vkp",
        "--timeout-ms",
        "20000",
        "--",
        "vkcube",
        "--wsi",
        "wayland",
    ]);
    let t = std::time::Instant::now();
    let out = h.run_with(&mut cmd);
    if out.code == 0 {
        eprintln!("this driver's buffers are CPU-mappable; pixman displays vkcube");
        assert_eq!(out.json["info"]["renderer"], "pixman");
    } else {
        assert_eq!(out.json["error"]["code"], "protocol_error", "{}", out.json);
        assert!(
            t.elapsed().as_secs() < 15,
            "the failure should not wait for the timeout"
        );
    }
}
