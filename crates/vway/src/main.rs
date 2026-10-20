//! `vway` CLI. Every command prints one JSON document on stdout (success and failure); failures
//! also print a one-line message on stderr. Exit status: 0 ok, 1 error, 2 usage.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand};
use serde_json::{Value, json};
use vway::actions::{ImageLimit, Images, Runner, Space, absolute, parse_script};
use vway::agents::{self, Agent};
use vway::client::Client;
use vway::opts::{SessionOpts, parse_env, parse_floats, parse_point, parse_size};
use vway::session::{self, OwnedSession, SpawnMode};
use vway_proto::{Button, ButtonAction, ErrorBody, ErrorCode, KeyAction, Op, WaitFor, err};

type Res = Result<Value, ErrorBody>;

#[derive(Parser)]
#[command(
    name = "vway",
    version,
    about = "Headless Wayland desktops for testing and driving GUI apps"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct Sel {
    /// Session id [default: $VWAY_SESSION, else the only running session]
    #[arg(short, long = "session", value_name = "ID")]
    session: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// One-shot: start a private session, launch CMD, run inline actions in order, stop
    Run(RunArgs),
    /// Start a detached session (optionally launching CMD)
    Start {
        #[command(flatten)]
        opts: SessionOpts,
        /// Session id [default: random]
        #[arg(long)]
        id: Option<String>,
        /// Stop a running session with the same id first
        #[arg(long)]
        replace: bool,
        /// How long to wait for CMD's first window
        #[arg(long, value_name = "MS")]
        timeout_ms: Option<u64>,
        #[arg(last = true, value_name = "CMD")]
        cmd: Vec<String>,
    },
    /// Launch an app in a session (one app per session)
    Launch {
        #[command(flatten)]
        sel: Sel,
        /// Environment override (K=V sets, K= unsets); repeatable
        #[arg(long = "env", value_name = "K=V")]
        env: Vec<String>,
        /// Working directory [default: current directory]
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Return right after spawning instead of waiting for the first window
        #[arg(long)]
        no_wait: bool,
        #[arg(long, value_name = "MS")]
        timeout_ms: Option<u64>,
        #[arg(last = true, required = true, value_name = "CMD")]
        cmd: Vec<String>,
    },
    /// Session status: output, pointer, windows, app, timing
    Status(Sel),
    /// Wait for a window, a frame, idleness, or app exit
    Wait {
        #[command(flatten)]
        sel: Sel,
        #[arg(value_parser = ["window", "frame", "idle", "exit"])]
        condition: String,
        #[arg(long, value_name = "MS")]
        timeout_ms: Option<u64>,
        /// window: match a title substring
        #[arg(long)]
        title: Option<String>,
        /// window: match an app id substring
        #[arg(long)]
        app_id: Option<String>,
        /// frame/idle: only count commits after this sequence number
        #[arg(long, value_name = "SEQ")]
        after_seq: Option<u64>,
        /// idle: quiet period without commits
        #[arg(long, value_name = "MS")]
        quiet_ms: Option<u64>,
    },
    /// Save a PNG screenshot
    Screenshot {
        #[command(flatten)]
        sel: Sel,
        /// Output file [default: <session dir>/shots/<time>.png]
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Crop to logical X,Y,W,H
        #[arg(long, value_name = "X,Y,W,H")]
        region: Option<String>,
        /// Do not wait (up to 3 s) for the app to go idle first
        #[arg(long)]
        no_settle: bool,
    },
    /// Move the pointer to X,Y (logical pixels)
    Move {
        #[command(flatten)]
        sel: Sel,
        #[arg(allow_hyphen_values = true, value_name = "X,Y")]
        point: String,
    },
    /// Click (optionally moving to X,Y first)
    Click {
        #[command(flatten)]
        sel: Sel,
        #[arg(allow_hyphen_values = true, value_name = "X,Y")]
        point: Option<String>,
        /// left | right | middle | back | forward | evdev code
        #[arg(long, default_value = "left")]
        button: String,
        /// 2 = double click, 3 = triple click
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// Modifiers held during the click, e.g. ctrl+shift
        #[arg(long, value_name = "MODS")]
        mods: Option<String>,
    },
    /// Press or release a mouse button
    Mouse {
        #[command(flatten)]
        sel: Sel,
        #[arg(value_parser = ["down", "up"])]
        action: String,
        #[arg(long, default_value = "left")]
        button: String,
    },
    /// Drag with the left button from X1,Y1 to X2,Y2
    Drag {
        #[command(flatten)]
        sel: Sel,
        #[arg(allow_hyphen_values = true, value_name = "X1,Y1")]
        from: String,
        #[arg(allow_hyphen_values = true, value_name = "X2,Y2")]
        to: String,
        #[arg(long, value_name = "MS", default_value_t = 300)]
        duration_ms: u64,
    },
    /// Scroll by wheel clicks
    Scroll {
        #[command(flatten)]
        sel: Sel,
        #[arg(value_parser = ["up", "down", "left", "right"])]
        direction: String,
        #[arg(default_value_t = 3.0)]
        amount: f64,
        /// Move the pointer here first
        #[arg(long, allow_hyphen_values = true, value_name = "X,Y")]
        at: Option<String>,
    },
    /// Press a key combo, e.g. ctrl+shift+e, Return, F5
    Key {
        #[command(flatten)]
        sel: Sel,
        keys: String,
        #[arg(long, value_name = "N")]
        repeat: Option<u32>,
        /// Only press (hold) the keys
        #[arg(long, conflicts_with = "up")]
        down: bool,
        /// Only release the keys
        #[arg(long)]
        up: bool,
    },
    /// Type text (`-` reads stdin)
    Type {
        #[command(flatten)]
        sel: Sel,
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
    /// Touch gestures
    Touch {
        #[command(flatten)]
        sel: Sel,
        #[command(subcommand)]
        gesture: Gesture,
    },
    /// Resize the output (logical WxH) and optionally change its scale
    Resize {
        #[command(flatten)]
        sel: Sel,
        #[arg(value_name = "WxH")]
        size: String,
        #[arg(long)]
        scale: Option<f64>,
    },
    /// Focus a window by id (see `status`)
    Focus {
        #[command(flatten)]
        sel: Sel,
        window: u32,
    },
    /// Ask the app to exit (SIGTERM), escalating to SIGKILL after the grace period
    CloseApp {
        #[command(flatten)]
        sel: Sel,
        #[arg(long, value_name = "MS")]
        grace_ms: Option<u64>,
    },
    /// Send xdg_toplevel.close to a window
    CloseWindow {
        #[command(flatten)]
        sel: Sel,
        window: u32,
    },
    /// Run a vway.action.v1 script (file or `-` for stdin)
    Do {
        #[command(flatten)]
        sel: Sel,
        script: String,
        /// Where screenshots without a path go [default: <session dir>/shots]
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Shrink screenshots to fit a model's image limit (coordinates follow the screenshots)
        #[arg(long, value_enum, default_value = "none")]
        image_limit: ImageLimit,
    },
    /// Answer Claude computer-use tool calls: tool_use blocks in (file or `-`), tool_result
    /// blocks out
    Computer {
        #[command(flatten)]
        sel: Sel,
        #[arg(default_value = "-")]
        input: String,
        /// [default: large for computer_toolset_20260801 calls, else standard]
        #[arg(long, value_enum)]
        image_limit: Option<ImageLimit>,
    },
    /// Stop a session (or all of them)
    Stop {
        #[command(flatten)]
        sel: Sel,
        #[arg(long, conflicts_with = "session")]
        all: bool,
    },
    /// List sessions
    List {
        /// Include stopped sessions
        #[arg(long)]
        all: bool,
    },
    /// Remove directories of stopped sessions
    Gc,
    /// Check the host for what vway needs, with install commands for what is missing
    Doctor,
    /// Make the vway skill (and optionally the MCP server) available to coding agents
    Agents {
        #[command(subcommand)]
        cmd: AgentsCmd,
    },
    /// Print the agent guide for this version (Markdown, not JSON)
    Guide {
        /// Print the SKILL.md that `agents setup` installs instead
        #[arg(long)]
        skill: bool,
    },
    /// MCP server on stdio
    Mcp {
        /// Only allow launching these programs (`dir/` = anything below it); repeatable
        #[arg(long, value_name = "PROGRAM|DIR/")]
        allow: Vec<String>,
        /// Shrink screenshots to fit the model's image limit (coordinates follow the screenshots)
        #[arg(long, value_enum, default_value = "standard")]
        image_limit: ImageLimit,
    },
    #[command(name = "__serve", hide = true)]
    Serve { dir: PathBuf },
}

#[derive(clap::Args)]
struct RunArgs {
    #[command(flatten)]
    opts: SessionOpts,
    /// Save a screenshot (inline action; repeatable, runs in command-line order)
    #[arg(long, value_name = "FILE")]
    screenshot: Vec<PathBuf>,
    /// Run a vway.action.v1 script (inline action)
    #[arg(long, value_name = "FILE")]
    script: Vec<PathBuf>,
    /// Left click at X,Y (inline action)
    #[arg(long, value_name = "X,Y", allow_hyphen_values = true)]
    click: Vec<String>,
    /// Press a key combo (inline action)
    #[arg(long, value_name = "KEYS")]
    key: Vec<String>,
    /// Type text (inline action)
    #[arg(long = "type", value_name = "TEXT", allow_hyphen_values = true)]
    type_text: Vec<String>,
    /// Sleep (inline action)
    #[arg(long, value_name = "MS")]
    wait_ms: Vec<u64>,
    /// After the actions, wait for the app to exit on its own
    #[arg(long)]
    until_exit: bool,
    /// How long to wait for the first window
    #[arg(long, value_name = "MS")]
    timeout_ms: Option<u64>,
    /// Where script screenshots without a path go
    #[arg(long, default_value = ".")]
    out_dir: PathBuf,
    /// Shrink screenshots to fit a model's image limit (coordinates follow the screenshots)
    #[arg(long, value_enum, default_value = "none")]
    image_limit: ImageLimit,
    /// Keep the session directory (logs) even on success
    #[arg(long)]
    keep: bool,
    #[arg(last = true, required = true, value_name = "CMD")]
    cmd: Vec<String>,
}

#[derive(Subcommand)]
enum AgentsCmd {
    /// Install the skill for the agents found here (safe to re-run)
    Setup {
        #[command(flatten)]
        to: AgentTargets,
        /// Also register `vway mcp` as an MCP server
        #[arg(long)]
        mcp: bool,
        /// MCP server command [default: `vway` if PATH runs this binary, else its absolute path]
        #[arg(long, value_name = "PROGRAM")]
        command: Option<String>,
        /// Replace a vway skill or MCP entry that `setup` did not create
        #[arg(long)]
        force: bool,
        /// Report what would change without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Show where the skill and MCP server are installed
    Status {
        #[command(flatten)]
        to: AgentTargets,
    },
    /// Remove what `setup` installed (skills it did not write are left alone)
    Remove {
        #[command(flatten)]
        to: AgentTargets,
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(clap::Args)]
struct AgentTargets {
    /// Use this project (default: the current directory) instead of your home directory
    #[arg(long, value_name = "DIR", num_args = 0..=1, default_missing_value = ".")]
    project: Option<PathBuf>,
    /// Only these agents: claude, codex, gemini, cursor, opencode, pi, dsh [default: setup uses
    /// the ones found here, status and remove all of them]
    #[arg(long = "agent", value_name = "NAME", value_delimiter = ',')]
    agents: Vec<String>,
}

#[derive(Subcommand)]
enum Gesture {
    /// Tap at X,Y
    Tap {
        #[arg(value_name = "X,Y")]
        point: String,
    },
    /// Press and hold at X,Y
    LongPress {
        #[arg(value_name = "X,Y")]
        point: String,
        #[arg(long, value_name = "MS", default_value_t = 800)]
        duration_ms: u64,
    },
    /// Swipe one finger from X1,Y1 to X2,Y2
    Swipe {
        #[arg(value_name = "X1,Y1")]
        from: String,
        #[arg(value_name = "X2,Y2")]
        to: String,
        #[arg(long, value_name = "MS", default_value_t = 300)]
        duration_ms: u64,
    },
    /// Two-finger pinch centred on X,Y (to < from zooms out)
    Pinch {
        #[arg(value_name = "X,Y")]
        point: String,
        #[arg(long, default_value_t = 200.0)]
        from: f64,
        #[arg(long, default_value_t = 80.0)]
        to: f64,
        #[arg(long, value_name = "MS", default_value_t = 400)]
        duration_ms: u64,
    },
}

fn main() {
    let matches = match Cli::command().try_get_matches() {
        Ok(m) => m,
        Err(e) => usage_exit(e),
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => usage_exit(e),
    };
    if let Cmd::Serve { dir } = &cli.cmd {
        std::process::exit(vway_compositor::serve(dir, Some(3)));
    }
    if let Cmd::Guide { skill } = cli.cmd {
        print!("{}", if skill { agents::SKILL } else { agents::GUIDE });
        std::process::exit(0);
    }
    if let Cmd::Mcp { allow, image_limit } = cli.cmd {
        let code = match session::state_root() {
            Ok(root) => match vway::mcp::Server::new(root, allow, image_limit).run() {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("vway mcp: {e}");
                    1
                }
            },
            Err(e) => {
                eprintln!("vway mcp: {e}");
                1
            }
        };
        std::process::exit(code);
    }
    let run_matches = matches.subcommand_matches("run");
    let code = match dispatch(cli.cmd, run_matches) {
        Ok(Outcome::Ok(v)) => {
            print_json(&with_ok(v, true));
            0
        }
        Ok(Outcome::Failed(v, msg)) => {
            print_json(&with_ok(v, false));
            eprintln!("vway: {msg}");
            1
        }
        Err(e) => {
            print_json(&json!({ "ok": false, "error": e }));
            eprintln!("vway: error[{}]: {}", e.code, e.message);
            if e.code == ErrorCode::Usage { 2 } else { 1 }
        }
    };
    std::process::exit(code);
}

fn usage_exit(e: clap::Error) -> ! {
    use clap::error::ErrorKind;
    if matches!(
        e.kind(),
        ErrorKind::DisplayHelp
            | ErrorKind::DisplayVersion
            | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    ) {
        let _ = e.print();
        std::process::exit(
            if e.kind() == ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand {
                2
            } else {
                0
            },
        );
    }
    let rendered = e.render().to_string();
    let first = rendered
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("usage error")
        .trim_start_matches("error: ")
        .to_string();
    print_json(&json!({ "ok": false, "error": err(ErrorCode::Usage, first) }));
    eprint!("{rendered}");
    std::process::exit(2);
}

fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

fn with_ok(v: Value, ok: bool) -> Value {
    match v {
        Value::Object(mut o) => {
            let mut out = serde_json::Map::new();
            out.insert("ok".into(), json!(ok));
            o.remove("ok");
            out.extend(o);
            Value::Object(out)
        }
        Value::Null => json!({ "ok": ok }),
        other => json!({ "ok": ok, "result": other }),
    }
}

/// A command can fail after producing a useful document (e.g. a script report).
enum Outcome {
    Ok(Value),
    Failed(Value, String),
}

fn connect(sel: &Sel) -> Result<(String, PathBuf, Client), ErrorBody> {
    let root = session::state_root()?;
    let (id, dir) = session::resolve(&root, sel.session.as_deref())?;
    let client = Client::connect(&dir)?;
    Ok((id, dir, client))
}

fn call(sel: &Sel, op: Op) -> Result<Outcome, ErrorBody> {
    let (_, _, mut c) = connect(sel)?;
    Ok(Outcome::Ok(c.call(&op)?))
}

fn point(s: &str) -> Result<(f64, f64), ErrorBody> {
    parse_point(s).map_err(|m| err(ErrorCode::Usage, m))
}

fn run_actions(
    sel: &Sel,
    actions: Vec<Value>,
    out_dir: Option<PathBuf>,
    limit: ImageLimit,
) -> Result<Outcome, ErrorBody> {
    let (_, dir, mut c) = connect(sel)?;
    let images = Images::Files(
        out_dir
            .map(|d| absolute(&d))
            .unwrap_or_else(|| dir.join("shots")),
    );
    let report = Runner::new(&mut c, images, Space::Screenshot(limit)).run(&actions);
    report_outcome(serde_json::to_value(&report).unwrap_or_default(), report.ok)
}

fn report_outcome(v: Value, ok: bool) -> Result<Outcome, ErrorBody> {
    if ok {
        return Ok(Outcome::Ok(v));
    }
    let msg = v["results"]
        .as_array()
        .and_then(|r| r.iter().find(|r| r["ok"] == json!(false)))
        .map(|r| {
            format!(
                "action {} ({}) failed: error[{}]: {}",
                r["index"],
                r["type"].as_str().unwrap_or("?"),
                r["error"]["code"].as_str().unwrap_or("?"),
                r["error"]["message"].as_str().unwrap_or("")
            )
        })
        .unwrap_or_else(|| "script failed".into());
    Ok(Outcome::Failed(v, msg))
}

/// The single action as a runner result (unwrapping the one-element report).
fn one_action(sel: &Sel, action: Value) -> Result<Outcome, ErrorBody> {
    let (_, _, mut c) = connect(sel)?;
    let mut runner = Runner::new(&mut c, Images::Inline, Space::Logical);
    Ok(Outcome::Ok(runner.run_one(&action)?))
}

fn read_script(src: &str) -> Result<Vec<Value>, ErrorBody> {
    let text = read_input(src)?;
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| err(ErrorCode::ScriptError, format!("{src}: invalid JSON: {e}")))?;
    parse_script(&v)
}

/// A file's contents, or stdin for `-`.
fn read_input(src: &str) -> Result<String, ErrorBody> {
    Ok(if src == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| err(ErrorCode::IoError, format!("reading stdin: {e}")))?;
        s
    } else {
        std::fs::read_to_string(src)
            .map_err(|e| err(ErrorCode::IoError, format!("reading {src}: {e}")))?
    })
}

fn launch_op(argv: Vec<String>, env: BTreeMap<String, Option<String>>, cwd: Option<PathBuf>) -> Op {
    let cwd = cwd
        .map(|c| absolute(&c))
        .or_else(|| std::env::current_dir().ok());
    Op::Launch {
        argv,
        base_env: Some(
            std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
        ),
        env,
        cwd: cwd.map(|c| c.to_string_lossy().into_owned()),
    }
}

fn wait_window(c: &mut Client, timeout_ms: Option<u64>) -> Res {
    c.call(&Op::Wait {
        condition: WaitFor::Window,
        timeout_ms,
        title: None,
        app_id: None,
        after_seq: None,
        quiet_ms: None,
    })
}

/// Attach the app log tail to launch/wait failures so the cause is visible in one place.
fn with_app_log(dir: &Path, e: ErrorBody) -> ErrorBody {
    let tail = session::log_tail(&dir.join("app.log"), 30);
    if tail.is_empty() {
        return e;
    }
    let mut d = e.details.clone().unwrap_or(json!({}));
    if d.is_object() {
        d["app_log_tail"] = json!(tail);
    }
    e.with_details(d)
}

fn dispatch(cmd: Cmd, run_matches: Option<&ArgMatches>) -> Result<Outcome, ErrorBody> {
    match cmd {
        Cmd::Run(args) => cmd_run(args, run_matches.expect("run matches")),
        Cmd::Start {
            opts,
            id,
            replace,
            timeout_ms,
            cmd,
        } => {
            let root = session::state_root()?;
            let cfg = opts.to_config(&root)?;
            let s = session::start(&root, cfg, id.as_deref(), replace, SpawnMode::Detached, "")?;
            let mut out = json!({ "session": s.id, "info": s.info });
            if cmd.is_empty() {
                return Ok(Outcome::Ok(out));
            }
            let r = (|| {
                let mut c = Client::connect(&s.dir)?;
                let launched = c.call(&launch_op(cmd, BTreeMap::new(), None))?;
                let waited =
                    wait_window(&mut c, timeout_ms).map_err(|e| with_app_log(&s.dir, e))?;
                Ok::<_, ErrorBody>((launched, waited))
            })();
            match r {
                Ok((launched, waited)) => {
                    out["launch"] = launched;
                    out["window"] = waited["window"].clone();
                    Ok(Outcome::Ok(out))
                }
                Err(e) => {
                    let msg = format!(
                        "session {} started, but the app failed: error[{}]: {}",
                        s.id, e.code, e.message
                    );
                    out["error"] = serde_json::to_value(&e).unwrap_or_default();
                    Ok(Outcome::Failed(out, msg))
                }
            }
        }
        Cmd::Launch {
            sel,
            env,
            cwd,
            no_wait,
            timeout_ms,
            cmd,
        } => {
            let env = parse_env(&env)?;
            let (_, dir, mut c) = connect(&sel)?;
            let mut out = c.call(&launch_op(cmd, env, cwd))?;
            if !no_wait {
                let waited = wait_window(&mut c, timeout_ms).map_err(|e| with_app_log(&dir, e))?;
                out["window"] = waited["window"].clone();
            }
            Ok(Outcome::Ok(out))
        }
        Cmd::Status(sel) => {
            let (id, dir, mut c) = connect(&sel)?;
            let mut v = c.call(&Op::Status)?;
            v["dir"] = json!(dir);
            v["session"] = json!(id);
            Ok(Outcome::Ok(v))
        }
        Cmd::Wait {
            sel,
            condition,
            timeout_ms,
            title,
            app_id,
            after_seq,
            quiet_ms,
        } => {
            let condition = match condition.as_str() {
                "window" => WaitFor::Window,
                "frame" => WaitFor::Frame,
                "idle" => WaitFor::Idle,
                _ => WaitFor::AppExit,
            };
            call(
                &sel,
                Op::Wait {
                    condition,
                    timeout_ms,
                    title,
                    app_id,
                    after_seq,
                    quiet_ms,
                },
            )
        }
        Cmd::Screenshot {
            sel,
            output,
            region,
            no_settle,
        } => {
            let (_, dir, mut c) = connect(&sel)?;
            let path = match output {
                Some(p) => absolute(&p),
                None => {
                    let shots = dir.join("shots");
                    std::fs::create_dir_all(&shots).map_err(|e| {
                        err(
                            ErrorCode::IoError,
                            format!("cannot create {}: {e}", shots.display()),
                        )
                    })?;
                    shots.join(format!("{}.png", vway_compositor::process::now_unix_ms()))
                }
            };
            let mut action = json!({ "type": "screenshot", "path": path, "settle": !no_settle });
            if let Some(r) = region {
                let r = parse_floats(&r, 4).map_err(|m| err(ErrorCode::Usage, m))?;
                action["region"] = json!([r[0], r[1], r[0] + r[2], r[1] + r[3]]);
            }
            Ok(Outcome::Ok(
                Runner::new(&mut c, Images::Inline, Space::Logical).run_one(&action)?,
            ))
        }
        Cmd::Move { sel, point: p } => {
            let (x, y) = point(&p)?;
            call(&sel, Op::PointerMove { x, y })
        }
        Cmd::Click {
            sel,
            point: p,
            button,
            count,
            mods,
        } => {
            let p = p.as_deref().map(point).transpose()?;
            let button = parse_button(&button);
            let (_, _, mut c) = connect(&sel)?;
            if let Some((x, y)) = p {
                c.call(&Op::PointerMove { x, y })?;
            }
            let mods: Vec<String> = mods
                .map(|m| {
                    m.split('+')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            let key = |c: &mut Client, k: &str, action| {
                c.call(&Op::Key {
                    keys: Some(k.into()),
                    code: None,
                    action,
                    repeat: None,
                })
            };
            let mut held = Vec::new();
            let mut r = Ok(json!({}));
            for m in &mods {
                r = key(&mut c, m, KeyAction::Down);
                if r.is_err() {
                    break;
                }
                held.push(m);
            }
            if r.is_ok() {
                r = c.call(&Op::PointerButton {
                    button,
                    action: ButtonAction::Click,
                    count: Some(count),
                });
            }
            for m in held.iter().rev() {
                let _ = key(&mut c, m, KeyAction::Up);
            }
            Ok(Outcome::Ok(r?))
        }
        Cmd::Mouse {
            sel,
            action,
            button,
        } => call(
            &sel,
            Op::PointerButton {
                button: parse_button(&button),
                action: if action == "down" {
                    ButtonAction::Down
                } else {
                    ButtonAction::Up
                },
                count: None,
            },
        ),
        Cmd::Drag {
            sel,
            from,
            to,
            duration_ms,
        } => {
            let (a, b) = (point(&from)?, point(&to)?);
            one_action(
                &sel,
                json!({ "type": "left_click_drag", "start_coordinate": [a.0, a.1],
                        "coordinate": [b.0, b.1], "duration_ms": duration_ms }),
            )
        }
        Cmd::Scroll {
            sel,
            direction,
            amount,
            at,
        } => {
            let mut action =
                json!({ "type": "scroll", "scroll_direction": direction, "scroll_amount": amount });
            if let Some(at) = at {
                let (x, y) = point(&at)?;
                action["coordinate"] = json!([x, y]);
            }
            one_action(&sel, action)
        }
        Cmd::Key {
            sel,
            keys,
            repeat,
            down,
            up,
        } => {
            let action = if down {
                KeyAction::Down
            } else if up {
                KeyAction::Up
            } else {
                KeyAction::Press
            };
            call(
                &sel,
                Op::Key {
                    keys: Some(keys),
                    code: None,
                    action,
                    repeat,
                },
            )
        }
        Cmd::Type { sel, text } => {
            let text = if text == "-" {
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .map_err(|e| err(ErrorCode::IoError, format!("reading stdin: {e}")))?;
                s
            } else {
                text
            };
            call(&sel, Op::Type { text })
        }
        Cmd::Touch { sel, gesture } => {
            let action = match gesture {
                Gesture::Tap { point: p } => {
                    let (x, y) = point(&p)?;
                    json!({ "type": "vway_gesture", "kind": "tap", "coordinate": [x, y] })
                }
                Gesture::LongPress {
                    point: p,
                    duration_ms,
                } => {
                    let (x, y) = point(&p)?;
                    json!({ "type": "vway_gesture", "kind": "long_press", "coordinate": [x, y], "duration_ms": duration_ms })
                }
                Gesture::Swipe {
                    from,
                    to,
                    duration_ms,
                } => {
                    let (a, b) = (point(&from)?, point(&to)?);
                    json!({ "type": "vway_gesture", "kind": "swipe", "start_coordinate": [a.0, a.1],
                            "coordinate": [b.0, b.1], "duration_ms": duration_ms })
                }
                Gesture::Pinch {
                    point: p,
                    from,
                    to,
                    duration_ms,
                } => {
                    let (x, y) = point(&p)?;
                    json!({ "type": "vway_gesture", "kind": "pinch", "coordinate": [x, y],
                            "from_distance": from, "to_distance": to, "duration_ms": duration_ms })
                }
            };
            one_action(&sel, action)
        }
        Cmd::Resize { sel, size, scale } => {
            let (width, height) = parse_size(&size).map_err(|m| err(ErrorCode::Usage, m))?;
            call(
                &sel,
                Op::Resize {
                    width,
                    height,
                    scale,
                },
            )
        }
        Cmd::Focus { sel, window } => call(&sel, Op::Focus { window }),
        Cmd::CloseApp { sel, grace_ms } => call(&sel, Op::CloseApp { grace_ms }),
        Cmd::CloseWindow { sel, window } => call(&sel, Op::CloseWindow { window }),
        Cmd::Do {
            sel,
            script,
            out_dir,
            image_limit,
        } => run_actions(&sel, read_script(&script)?, out_dir, image_limit),
        Cmd::Computer {
            sel,
            input,
            image_limit,
        } => {
            let text = read_input(&input)?;
            let v: Value = serde_json::from_str(&text).map_err(|e| {
                err(
                    ErrorCode::ScriptError,
                    format!("{input}: invalid JSON: {e}"),
                )
            })?;
            let calls = vway::computer::calls(&v)?;
            let limit = image_limit.unwrap_or_else(|| vway::computer::default_limit(&calls));
            let (_, _, mut c) = connect(&sel)?;
            let mut runner = Runner::new(&mut c, Images::Inline, Space::Screenshot(limit));
            let (results, ok) = vway::computer::answer(&mut runner, &calls);
            let v = json!({ "ok": ok, "tool_results": results });
            if ok {
                Ok(Outcome::Ok(v))
            } else {
                let msg = results
                    .iter()
                    .find(|r| r["is_error"] == true)
                    .and_then(|r| r["content"][0]["text"].as_str())
                    .unwrap_or("a computer action failed")
                    .to_string();
                Ok(Outcome::Failed(v, msg))
            }
        }
        Cmd::Stop { sel, all } => {
            let root = session::state_root()?;
            if all {
                let mut stopped = Vec::new();
                for e in session::list(&root, false) {
                    let exit = session::stop(Path::new(&e.dir), Duration::from_secs(5))?;
                    stopped.push(json!({ "session": e.id, "exit": exit }));
                }
                return Ok(Outcome::Ok(json!({ "stopped": stopped })));
            }
            let (id, dir) = session::resolve(&root, sel.session.as_deref())?;
            let exit = session::stop(&dir, Duration::from_secs(5))?;
            Ok(Outcome::Ok(json!({ "session": id, "exit": exit })))
        }
        Cmd::List { all } => {
            let root = session::state_root()?;
            Ok(Outcome::Ok(
                json!({ "sessions": session::list(&root, all) }),
            ))
        }
        Cmd::Gc => {
            let root = session::state_root()?;
            Ok(Outcome::Ok(json!({ "removed": session::gc(&root) })))
        }
        Cmd::Doctor => doctor(),
        Cmd::Agents { cmd } => cmd_agents(cmd),
        Cmd::Mcp { .. } | Cmd::Serve { .. } | Cmd::Guide { .. } => {
            unreachable!("handled in main")
        }
    }
}

fn parse_button(s: &str) -> Button {
    s.parse::<u32>()
        .map(Button::Code)
        .unwrap_or_else(|_| Button::Name(s.into()))
}

/// Inline `run` actions, in command-line order.
fn inline_actions(args: &RunArgs, m: &ArgMatches) -> Result<Vec<Value>, ErrorBody> {
    let mut items: Vec<(usize, Vec<Value>)> = Vec::new();
    let mut push = |id: &str, f: &mut dyn FnMut(usize) -> Result<Vec<Value>, ErrorBody>| {
        for (n, idx) in m.indices_of(id).into_iter().flatten().enumerate() {
            items.push((idx, f(n)?));
        }
        Ok::<_, ErrorBody>(())
    };
    push("screenshot", &mut |n| {
        Ok(vec![
            json!({ "type": "screenshot", "path": absolute(&args.screenshot[n]) }),
        ])
    })?;
    push("script", &mut |n| {
        read_script(&args.script[n].to_string_lossy())
    })?;
    push("click", &mut |n| {
        let (x, y) = point(&args.click[n])?;
        Ok(vec![json!({ "type": "left_click", "coordinate": [x, y] })])
    })?;
    push("key", &mut |n| {
        Ok(vec![json!({ "type": "key", "text": args.key[n] })])
    })?;
    push("type_text", &mut |n| {
        Ok(vec![json!({ "type": "type", "text": args.type_text[n] })])
    })?;
    push("wait_ms", &mut |n| {
        Ok(vec![
            json!({ "type": "wait", "duration": args.wait_ms[n] as f64 / 1000.0 }),
        ])
    })?;
    items.sort_by_key(|(idx, _)| *idx);
    Ok(items.into_iter().flat_map(|(_, a)| a).collect())
}

fn cmd_run(args: RunArgs, m: &ArgMatches) -> Result<Outcome, ErrorBody> {
    let actions = inline_actions(&args, m)?;
    let root = session::state_root()?;
    let cfg = args.opts.to_config(&root)?;
    let mut s = OwnedSession::start(&root, cfg, "run-")?;
    s.keep_dir = args.keep;
    let mut out = json!({ "session": s.id });

    let result = (|| -> Result<Option<String>, ErrorBody> {
        let mut c = Client::connect(&s.dir)?;
        out["launch"] = c.call(&launch_op(args.cmd.clone(), BTreeMap::new(), None))?;
        let waited = wait_window(&mut c, args.timeout_ms).map_err(|e| with_app_log(&s.dir, e))?;
        out["window"] = waited["window"].clone();
        if !actions.is_empty() {
            let images = Images::Files(absolute(&args.out_dir));
            let report =
                Runner::new(&mut c, images, Space::Screenshot(args.image_limit)).run(&actions);
            let v = serde_json::to_value(&report).unwrap_or_default();
            out["results"] = v["results"].clone();
            if !report.ok
                && let Ok(Outcome::Failed(_, msg)) = report_outcome(v, false)
            {
                return Ok(Some(msg));
            }
        }
        if args.until_exit {
            loop {
                match c.call(&Op::Wait {
                    condition: WaitFor::AppExit,
                    timeout_ms: Some(60_000),
                    title: None,
                    app_id: None,
                    after_seq: None,
                    quiet_ms: None,
                }) {
                    Ok(_) => break,
                    Err(e) if e.code == ErrorCode::Timeout => continue,
                    Err(e) => return Err(e),
                }
            }
        }
        out["app"] = serde_json::to_value(c.status()?.app).unwrap_or_default();
        Ok(None)
    })();

    let failed = !matches!(result, Ok(None));
    if failed {
        s.keep_dir = true;
    }
    let dir = s.dir.clone();
    let exit = s.stop();
    out["exit"] = serde_json::to_value(&exit).unwrap_or_default();
    if failed {
        out["logs"] = json!({
            "dir": dir,
            "app": dir.join("app.log"),
            "compositor": dir.join("compositor.log"),
        });
    }
    match result {
        Ok(None) => Ok(Outcome::Ok(out)),
        Ok(Some(msg)) => Ok(Outcome::Failed(out, msg)),
        Err(e) => {
            let msg = format!("error[{}]: {}", e.code, e.message);
            out["error"] = serde_json::to_value(&e).unwrap_or_default();
            Ok(Outcome::Failed(out, msg))
        }
    }
}

fn cmd_agents(cmd: AgentsCmd) -> Result<Outcome, ErrorBody> {
    let (to, mode, command, dry_run) = match cmd {
        AgentsCmd::Setup {
            to,
            mcp,
            command,
            force,
            dry_run,
        } => (to, agents::Mode::Setup { mcp, force }, command, dry_run),
        AgentsCmd::Status { to } => (to, agents::Mode::Status, None, false),
        AgentsCmd::Remove { to, dry_run } => (to, agents::Mode::Remove, None, dry_run),
    };
    let only = to
        .agents
        .iter()
        .map(|a| {
            Agent::parse(a).ok_or_else(|| {
                err(
                    ErrorCode::Usage,
                    format!(
                        "unknown agent {a:?} (claude, codex, gemini, cursor, opencode, pi, dsh)"
                    ),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(p) = &to.project
        && !p.is_dir()
    {
        return Err(err(
            ErrorCode::IoError,
            format!("{} is not a directory", p.display()),
        ));
    }
    let ctx = agents::Ctx::from_env(to.project)?;
    let report = agents::run(&ctx, mode, &only, command, dry_run);
    if report.failed > 0 {
        let msg = format!("{} change(s) failed; see items", report.failed);
        return Ok(Outcome::Failed(report.value, msg));
    }
    Ok(Outcome::Ok(report.value))
}

fn doctor() -> Result<Outcome, ErrorBody> {
    let (v, ok) = vway::doctor::run();
    if ok {
        Ok(Outcome::Ok(v))
    } else {
        Ok(Outcome::Failed(v, "required checks failed".into()))
    }
}
