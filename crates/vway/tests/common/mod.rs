//! Shared harness for the end-to-end tests: a private state root per test, the `vway` binary,
//! the test client, its JSON event log, and PNG pixel checks.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;

pub fn vway_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vway"))
}

/// `target/<profile>/examples/vway-testclient`, built on demand if `cargo test` did not build it.
pub fn testclient() -> PathBuf {
    let bin = vway_bin();
    let path = bin
        .parent()
        .expect("bin dir")
        .join("examples")
        .join("vway-testclient");
    if !path.exists() {
        let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["build", "--example", "vway-testclient", "-p", "vway"])
            .status()
            .expect("run cargo build --example vway-testclient");
        assert!(status.success(), "building the test client failed");
    }
    assert!(
        path.exists(),
        "test client not found at {} (run `cargo build --example vway-testclient`)",
        path.display()
    );
    path
}

pub fn tc() -> String {
    testclient().to_string_lossy().into_owned()
}

pub struct Out {
    pub code: i32,
    pub json: Value,
    pub stderr: String,
}

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A private `VWAY_STATE_DIR`; every session in it is stopped and the directory removed on drop.
pub struct Harness {
    pub root: PathBuf,
}

impl Harness {
    pub fn new() -> Harness {
        // Short on purpose: socket paths must fit in sun_path.
        let root = PathBuf::from(format!(
            "/tmp/vwt-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create state root");
        Harness { root }
    }

    pub fn cmd(&self) -> Command {
        let mut c = Command::new(vway_bin());
        c.env("VWAY_STATE_DIR", &self.root)
            .env_remove("VWAY_SESSION")
            .env_remove("VWAY_RENDERER");
        c
    }

    pub fn run(&self, args: &[&str]) -> Out {
        self.run_with(self.cmd().args(args))
    }

    pub fn run_with(&self, cmd: &mut Command) -> Out {
        Harness::parse(cmd)
    }

    /// Run any `vway` command line and check the output contract.
    pub fn parse(cmd: &mut Command) -> Out {
        let out: Output = cmd.stdin(Stdio::null()).output().expect("spawn vway");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let json = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!(
                "stdout is not one JSON document ({e}):\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        });
        assert!(
            stdout.starts_with("{\n  \"ok\": "),
            "\"ok\" must be the first key:\n{stdout}"
        );
        Out {
            code: out.status.code().unwrap_or(-1),
            json,
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Run and require success; returns the JSON document.
    pub fn ok(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.code == 0 && out.json["ok"] == true,
            "vway {args:?} failed (exit {}):\n{}\nstderr: {}",
            out.code,
            serde_json::to_string_pretty(&out.json).unwrap(),
            out.stderr
        );
        out.json
    }

    /// Run and require a failure with this error code; returns the JSON document.
    pub fn fails(&self, args: &[&str], code: &str) -> Value {
        let out = self.run(args);
        assert_ne!(
            out.code, 0,
            "vway {args:?} unexpectedly succeeded: {}",
            out.json
        );
        assert_eq!(
            out.json["error"]["code"], code,
            "vway {args:?}: unexpected error: {}",
            out.json
        );
        assert!(
            !out.stderr.is_empty(),
            "failures must explain themselves on stderr"
        );
        out.json
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn session_dir(&self, id: &str) -> PathBuf {
        self.root.join("sessions").join(id)
    }

    /// Start a detached session `id` running the test client with `args`; returns `start` output.
    pub fn start_tc(&self, id: &str, extra: &[&str], tc_args: &[&str]) -> Value {
        let tc = tc();
        let mut args = vec!["start", "--id", id];
        args.extend_from_slice(extra);
        args.push("--");
        args.push(&tc);
        args.extend_from_slice(tc_args);
        self.ok(&args)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.cmd().args(["stop", "--all"]).output();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `VWAY_TEST` events from a test client log.
pub fn events(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("VWAY_TEST "))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Poll the log until `pred` holds for the event list.
pub fn wait_events(log: &Path, what: &str, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ev = events(log);
        if pred(&ev) {
            return ev;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {what}; events:\n{}",
                ev.iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn of<'a>(ev: &'a [Value], name: &str) -> Vec<&'a Value> {
    ev.iter().filter(|e| e["event"] == name).collect()
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    rgba: Vec<u8>,
}

impl Image {
    pub fn load(path: &Path) -> Image {
        let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut reader = png::Decoder::new(std::io::BufReader::new(file))
            .read_info()
            .expect("png header");
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("png frame");
        assert_eq!(info.color_type, png::ColorType::Rgba);
        buf.truncate(info.buffer_size());
        Image {
            width: info.width,
            height: info.height,
            rgba: buf,
        }
    }

    pub fn rgb(&self, x: u32, y: u32) -> u32 {
        let i = ((y * self.width + x) * 4) as usize;
        u32::from_be_bytes([0, self.rgba[i], self.rgba[i + 1], self.rgba[i + 2]])
    }

    #[track_caller]
    pub fn assert_rgb(&self, x: u32, y: u32, want: u32) {
        let got = self.rgb(x, y);
        assert_eq!(got, want, "pixel {x},{y}: got {got:06x}, want {want:06x}");
    }

    /// Number of distinct colors (a blank or solid frame has one).
    pub fn colors(&self) -> usize {
        let mut seen: Vec<u32> = (0..self.height)
            .flat_map(|y| (0..self.width).map(move |x| (x, y)))
            .map(|(x, y)| self.rgb(x, y))
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }

    /// Number of pixels that differ from `other` (same size required).
    pub fn diff(&self, other: &Image) -> usize {
        assert_eq!((self.width, self.height), (other.width, other.height));
        self.rgba
            .as_chunks::<4>()
            .0
            .iter()
            .zip(other.rgba.as_chunks::<4>().0)
            .filter(|(a, b)| a != b)
            .count()
    }
}

/// The program's path on `PATH`, if installed.
pub fn which(prog: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(prog))
        .find(|p| p.is_file())
}

pub fn pid_alive(pid: i64) -> bool {
    // Zombies count as dead: they are only waiting for their (possibly new) parent to reap them.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z')),
        Err(_) => false,
    }
}

pub fn wait_until(what: &str, timeout: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
