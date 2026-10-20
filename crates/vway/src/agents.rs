//! `vway agents`: make the vway skill, and optionally its MCP server, available to coding agents.
//!
//! Package managers cannot write into the user's home, so this is an explicit, idempotent step.
//! Skills use the Agent Skills layout (`<root>/vway/SKILL.md`): `.agents/skills` is read by Codex,
//! Cursor, Gemini CLI, OpenCode, pi and dsh, `.claude/skills` (or `$CLAUDE_CONFIG_DIR/skills`) by Claude
//! Code. The installed skill is a short stub that sends the agent to `vway guide`, so it does not
//! go stale when vway is upgraded.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use serde_json::{Value, json};
use vway_proto::{ErrorBody, ErrorCode, err};

/// The skill stub (also shipped as `skills/vway/SKILL.md`).
pub const SKILL: &str = include_str!("../agent/SKILL.md");
/// The full agent guide, printed by `vway guide`.
pub const GUIDE: &str = include_str!("../agent/guide.md");

const NAME: &str = "vway";
const MARKER: &str = "<!-- installed by `vway agents setup`; `vway agents remove` deletes it -->";

/// The SKILL.md that `setup` writes: the stub, where this vway is, and the ownership marker.
/// Agents started by desktop apps often get a shorter PATH than a terminal, so the stub alone
/// can leave them unable to find `vway`.
pub fn installed_skill(program: &Path) -> String {
    format!(
        "{SKILL}\nIf `vway` is not on PATH (agents started from desktop apps often get a shorter PATH \
         than a terminal), run it as `{}`.\n\n{MARKER}\n",
        program.display()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Gemini,
    Cursor,
    Opencode,
    Pi,
    Dsh,
}

impl Agent {
    pub const ALL: [Agent; 7] = [
        Agent::Claude,
        Agent::Codex,
        Agent::Gemini,
        Agent::Cursor,
        Agent::Opencode,
        Agent::Pi,
        Agent::Dsh,
    ];

    pub fn parse(s: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|a| a.name() == s)
    }

    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Gemini => "gemini",
            Agent::Cursor => "cursor",
            Agent::Opencode => "opencode",
            Agent::Pi => "pi",
            Agent::Dsh => "dsh",
        }
    }

    fn cli(self) -> &'static str {
        match self {
            Agent::Cursor => "cursor-agent",
            a => a.name(),
        }
    }

    fn config_dir(self, ctx: &Ctx) -> PathBuf {
        match self {
            Agent::Claude => ctx.claude_home(),
            Agent::Codex => ctx.codex_home.clone(),
            Agent::Gemini => ctx.home.join(".gemini"),
            Agent::Cursor => ctx.home.join(".cursor"),
            Agent::Opencode => ctx.config_home.join("opencode"),
            Agent::Pi => ctx.pi_home.clone(),
            Agent::Dsh => ctx.dsh_home.clone(),
        }
    }
}

/// The environment `vway agents` works in.
pub struct Ctx {
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub codex_home: PathBuf,
    /// `$CLAUDE_CONFIG_DIR`, when set; Claude Code then keeps `.claude.json` there too.
    pub claude_config_dir: Option<PathBuf>,
    pub pi_home: PathBuf,
    pub dsh_home: PathBuf,
    pub path: Vec<PathBuf>,
    /// The desktop session's PATH, when a systemd user manager can be asked.
    pub desktop_path: Option<Vec<PathBuf>>,
    /// `Some` for `--project`: install into this directory instead of the home.
    pub project: Option<PathBuf>,
}

impl Ctx {
    pub fn from_env(project: Option<PathBuf>) -> Result<Ctx, ErrorBody> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| err(ErrorCode::IoError, "HOME is not set"))?;
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let project = match project {
            Some(p) => Some(
                std::path::absolute(&p)
                    .map_err(|e| err(ErrorCode::IoError, format!("{}: {e}", p.display())))?,
            ),
            None => None,
        };
        Ok(Ctx {
            config_home: var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
            codex_home: var("CODEX_HOME").unwrap_or_else(|| home.join(".codex")),
            claude_config_dir: var("CLAUDE_CONFIG_DIR"),
            pi_home: var("PI_CODING_AGENT_DIR").unwrap_or_else(|| home.join(".pi/agent")),
            dsh_home: var("DSH_HOME").unwrap_or_else(|| home.join(".dsh")),
            path: std::env::var_os("PATH")
                .map(|p| std::env::split_paths(&p).collect())
                .unwrap_or_default(),
            desktop_path: desktop_path(),
            home,
            project,
        })
    }

    fn base(&self) -> &Path {
        self.project.as_deref().unwrap_or(&self.home)
    }

    fn claude_home(&self) -> PathBuf {
        self.claude_config_dir
            .clone()
            .unwrap_or_else(|| self.home.join(".claude"))
    }

    fn which(&self, prog: &str) -> Option<PathBuf> {
        which_in(&self.path, prog)
    }

    pub fn detected(&self, a: Agent) -> bool {
        self.which(a.cli()).is_some() || a.config_dir(self).is_dir()
    }
}

/// The first executable `prog` in `path`.
pub fn which_in(path: &[PathBuf], prog: &str) -> Option<PathBuf> {
    which_all(path, prog).into_iter().next()
}

fn which_all(path: &[PathBuf], prog: &str) -> Vec<PathBuf> {
    path.iter()
        .map(|d| d.join(prog))
        .filter(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .collect()
}

/// Where this binary is, and how agents should invoke it.
#[derive(Debug, Serialize)]
pub struct Location {
    pub exe: PathBuf,
    /// Every `vway` on PATH, in PATH order.
    pub on_path: Vec<PathBuf>,
    /// Whether running `vway` from PATH runs this binary.
    pub path_runs_this: bool,
    /// The command to register for MCP: `vway`, a mise shim, or an absolute path.
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// The systemd user manager's PATH, which apps started from the desktop get, if there is one to
/// ask.
pub fn desktop_path() -> Option<Vec<PathBuf>> {
    let out = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let env = String::from_utf8_lossy(&out.stdout);
    let path = env.lines().find_map(|l| l.strip_prefix("PATH="))?;
    Some(std::env::split_paths(path).collect())
}

pub fn locate(path: &[PathBuf]) -> Location {
    let exe = std::env::current_exe()
        .and_then(|e| e.canonicalize())
        .unwrap_or_else(|_| PathBuf::from(NAME));
    let on_path = which_all(path, NAME);
    // mise installs look like <data>/installs/<tool>/<version>/bin/vway; its shims live in
    // <data>/shims and keep working across upgrades and without `mise activate`.
    let mise_shim = exe
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "installs"))
        .and_then(Path::parent)
        .map(|data| data.join("shims").join(NAME))
        .filter(|s| s.exists());
    let first = on_path.first();
    let first_real = first.and_then(|f| f.canonicalize().ok());
    let is_shim = first_real
        .as_ref()
        .is_some_and(|r| r.file_name().is_some_and(|n| n == "mise"));
    let path_runs_this = first_real.as_ref() == Some(&exe) || (is_shim && mise_shim.is_some());
    let shown = |p: &Path| p.to_string_lossy().into_owned();
    let (command, warning) = match (first, &mise_shim) {
        (_, Some(shim)) if path_runs_this => (shown(shim), None),
        (Some(_), None) if path_runs_this => (NAME.to_string(), None),
        (None, _) => (
            shown(&exe),
            Some(format!(
                "vway is not on PATH; agents get the absolute path {} (run setup again after \
                 moving or upgrading it)",
                exe.display()
            )),
        ),
        (Some(f), _) => (
            shown(&exe),
            Some(format!(
                "`vway` on PATH is {}, not this binary ({}); agents get this binary's path",
                f.display(),
                exe.display()
            )),
        ),
    };
    Location {
        exe,
        on_path,
        path_runs_this,
        command,
        warning,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Setup { mcp: bool, force: bool },
    Remove,
    Status,
}

/// One skill directory or MCP registration, before and after.
#[derive(Debug, Serialize)]
pub struct Item {
    pub kind: &'static str,
    pub agents: Vec<Agent>,
    pub target: String,
    /// skill: missing | current | outdated | foreign;
    /// mcp: missing | registered | different | unreadable
    pub state: &'static str,
    /// created | updated | replaced | removed | unchanged | skipped | failed (not for status)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub struct Report {
    pub value: Value,
    pub failed: usize,
}

pub fn run(
    ctx: &Ctx,
    mode: Mode,
    only: &[Agent],
    command: Option<String>,
    dry_run: bool,
) -> Report {
    let detected: Vec<Agent> = Agent::ALL
        .into_iter()
        .filter(|a| ctx.detected(*a))
        .collect();
    let selected: Vec<Agent> = match (mode, only.is_empty()) {
        (_, false) => only.to_vec(),
        (Mode::Setup { .. }, true) => detected.clone(),
        _ => Agent::ALL.to_vec(),
    };
    let mut loc = locate(&ctx.path);
    // Apps started from the desktop, and the agents they run, resolve a bare `vway` on the
    // session manager's PATH, not on this shell's.
    if loc.command == NAME
        && let Some(desktop) = &ctx.desktop_path
        && which_in(desktop, NAME).is_none()
    {
        loc.command = loc.exe.to_string_lossy().into_owned();
        loc.warning = Some(format!(
            "vway is not on the desktop session's PATH; agents get the absolute path {} (run \
             setup again after moving it)",
            loc.exe.display()
        ));
    }
    let command = command.unwrap_or_else(|| loc.command.clone());
    // `vway` itself says nothing to an agent whose PATH lacks it.
    let content = installed_skill(if command == NAME {
        &loc.exe
    } else {
        Path::new(&command)
    });
    let mut warnings: Vec<String> = Vec::new();
    if matches!(mode, Mode::Setup { mcp: true, .. }) {
        warnings.extend(loc.warning.clone());
    }

    let mut items = Vec::new();
    // Skills: one shared directory for every agent that reads `.agents/skills`, one for Claude.
    let shared: Vec<Agent> = selected
        .iter()
        .copied()
        .filter(|a| *a != Agent::Claude)
        .collect();
    let want_shared = !shared.is_empty() || selected.is_empty();
    if want_shared {
        let dir = ctx.base().join(".agents/skills").join(NAME);
        items.push(skill(&dir, &content, shared, mode, dry_run));
    }
    if selected.contains(&Agent::Claude) {
        let dir = match &ctx.project {
            Some(p) => p.join(".claude/skills"),
            None => ctx.claude_home().join("skills"),
        }
        .join(NAME);
        items.push(skill(&dir, &content, vec![Agent::Claude], mode, dry_run));
    }
    let do_mcp = !matches!(mode, Mode::Setup { mcp: false, .. });
    if do_mcp {
        for a in &selected {
            if let Some(item) = mcp(ctx, *a, &command, mode, dry_run) {
                items.push(item);
            } else if matches!(mode, Mode::Setup { mcp: true, .. })
                && config(ctx, *a, &command).is_none()
            {
                warnings.push(format!(
                    "{}: no MCP registration; it uses the skill",
                    a.name()
                ));
            }
        }
    }
    if matches!(mode, Mode::Setup { .. }) && detected.is_empty() && only.is_empty() {
        warnings.push(
            "no agent found (claude, codex, gemini, cursor-agent, opencode, pi, dsh on PATH or \
             their config directories); installed the skill in the shared .agents/skills only"
                .into(),
        );
    }
    for i in &items {
        if i.action == Some("skipped") {
            warnings.push(format!(
                "skipped {}: {}",
                i.target,
                i.detail.as_deref().unwrap_or("")
            ));
        }
    }
    let failed = items.iter().filter(|i| i.action == Some("failed")).count();
    let mut value = json!({
        "scope": if ctx.project.is_some() { "project" } else { "user" },
        "base": ctx.base(),
        "detected": detected,
        "agents": selected,
    });
    if do_mcp {
        value["mcp_command"] = json!([command, "mcp"]);
    }
    if mode != Mode::Status {
        value["dry_run"] = json!(dry_run);
    }
    value["items"] = json!(items);
    if !warnings.is_empty() {
        value["warnings"] = json!(warnings);
    }
    Report { value, failed }
}

// ---- skills -------------------------------------------------------------------------------------

fn skill_state(dir: &Path, content: &str) -> &'static str {
    match std::fs::symlink_metadata(dir) {
        Err(_) => "missing",
        Ok(m) if m.file_type().is_symlink() || !m.is_dir() => "foreign",
        Ok(_) => match std::fs::read_to_string(dir.join("SKILL.md")) {
            Ok(s) if s == content => "current",
            Ok(s) if s.contains(MARKER) => "outdated",
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing",
            _ => "foreign",
        },
    }
}

fn skill(dir: &Path, content: &str, agents: Vec<Agent>, mode: Mode, dry_run: bool) -> Item {
    let state = skill_state(dir, content);
    let mut item = Item {
        kind: "skill",
        agents,
        target: dir.join("SKILL.md").to_string_lossy().into_owned(),
        state,
        action: None,
        detail: None,
    };
    let action = match (mode, state) {
        (Mode::Status, _) => return item,
        (Mode::Setup { .. }, "current") => "unchanged",
        (Mode::Setup { .. }, "missing") => "created",
        (Mode::Setup { .. }, "outdated") => "updated",
        (Mode::Setup { force: true, .. }, _) => "replaced",
        (Mode::Setup { .. }, _) => {
            item.detail = Some(
                "a skill named vway that `vway agents setup` did not install is in the way \
                 (--force replaces it)"
                    .into(),
            );
            "skipped"
        }
        (Mode::Remove, "current" | "outdated") => "removed",
        (Mode::Remove, "missing") => "unchanged",
        (Mode::Remove, _) => {
            item.detail = Some("not installed by `vway agents setup`; left alone".into());
            "skipped"
        }
    };
    item.action = Some(action);
    let result = match action {
        _ if dry_run => Ok(()),
        "created" | "updated" => write_skill(dir, content),
        "replaced" => remove_any(dir).and_then(|()| write_skill(dir, content)),
        "removed" => remove_skill(dir),
        _ => Ok(()),
    };
    if let Err(e) = result {
        item.action = Some("failed");
        item.detail = Some(e.to_string());
    }
    item
}

fn write_skill(dir: &Path, content: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    write_atomic(&dir.join("SKILL.md"), content.as_bytes())
}

fn remove_skill(dir: &Path) -> std::io::Result<()> {
    std::fs::remove_file(dir.join("SKILL.md"))?;
    // Leave the directory if someone put other files in it.
    let _ = std::fs::remove_dir(dir);
    Ok(())
}

fn remove_any(p: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
        Ok(_) => std::fs::remove_file(p),
        Err(_) => Ok(()),
    }
}

/// Write via a temporary file and rename, keeping the permissions of the file it replaces.
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_file_name(format!(
        ".{}.vway-tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&tmp, data)?;
    if let Ok(m) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, m.permissions())?;
    }
    std::fs::rename(&tmp, path)
}

// ---- MCP registrations --------------------------------------------------------------------------

enum Config {
    /// Claude Code keeps user-scope servers in `~/.claude.json`, which it rewrites while running;
    /// read it, but change it only through `claude mcp`.
    ClaudeUser(PathBuf),
    /// A JSON file with a table of servers keyed by name.
    Json {
        file: PathBuf,
        table: &'static str,
        entry: Value,
    },
    /// Codex's `config.toml`, edited as text: one `[mcp_servers.vway]` table.
    Toml(PathBuf),
}

/// `None` for agents that use vway only through the skill: dsh's MCP servers are configured
/// through its plugin patches, not a file of servers.
fn config(ctx: &Ctx, a: Agent, command: &str) -> Option<Config> {
    let std_entry = json!({ "command": command, "args": ["mcp"] });
    let json = |file: PathBuf, table, entry| Config::Json { file, table, entry };
    Some(match (a, &ctx.project) {
        (Agent::Dsh, _) => return None,
        (Agent::Pi, None) => json(ctx.pi_home.join("mcp.json"), "mcpServers", std_entry),
        (Agent::Pi, Some(p)) => json(p.join(".pi/mcp.json"), "mcpServers", std_entry),
        (Agent::Claude, None) => Config::ClaudeUser(
            ctx.claude_config_dir
                .as_deref()
                .unwrap_or(&ctx.home)
                .join(".claude.json"),
        ),
        (Agent::Claude, Some(p)) => json(
            p.join(".mcp.json"),
            "mcpServers",
            json!({ "type": "stdio", "command": command, "args": ["mcp"] }),
        ),
        (Agent::Codex, None) => Config::Toml(ctx.codex_home.join("config.toml")),
        (Agent::Codex, Some(p)) => Config::Toml(p.join(".codex/config.toml")),
        (Agent::Gemini, None) => json(
            ctx.home.join(".gemini/settings.json"),
            "mcpServers",
            std_entry,
        ),
        (Agent::Gemini, Some(p)) => json(p.join(".gemini/settings.json"), "mcpServers", std_entry),
        (Agent::Cursor, None) => json(ctx.home.join(".cursor/mcp.json"), "mcpServers", std_entry),
        (Agent::Cursor, Some(p)) => json(p.join(".cursor/mcp.json"), "mcpServers", std_entry),
        (Agent::Opencode, _) => json(
            match &ctx.project {
                Some(p) => p.join("opencode.json"),
                None => ctx.config_home.join("opencode/opencode.json"),
            },
            "mcp",
            json!({ "type": "local", "command": [command, "mcp"], "enabled": true }),
        ),
    })
}

/// The command line of an existing entry, in any of the shapes above.
fn entry_argv(e: &Value) -> Option<Vec<String>> {
    let strs = |v: &Value| -> Option<Vec<String>> {
        v.as_array()?
            .iter()
            .map(|s| s.as_str().map(String::from))
            .collect()
    };
    match &e["command"] {
        Value::String(c) => {
            let mut argv = vec![c.clone()];
            argv.extend(strs(&e["args"]).unwrap_or_default());
            Some(argv)
        }
        v @ Value::Array(_) => strs(v),
        _ => None,
    }
}

/// Existing registration: `Ok(None)` when absent, `Err` when the file cannot be read as JSON.
fn read_json_entry(file: &Path, table: &str) -> Result<Option<Value>, String> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let jsonc = file.with_extension("jsonc");
            if jsonc.exists() {
                return Err(format!(
                    "{} exists; vway only edits plain JSON",
                    jsonc.display()
                ));
            }
            return Ok(None);
        }
        Err(e) => return Err(e.to_string()),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| format!("not plain JSON ({e}); vway does not edit files with comments"))?;
    if !v.is_object() {
        return Err("the top level is not a JSON object".into());
    }
    Ok(v.get(table).and_then(|t| t.get(NAME)).cloned())
}

fn write_json_entry(file: &Path, table: &str, entry: Option<&Value>) -> std::io::Result<()> {
    let mut root: Value = match std::fs::read_to_string(file) {
        Ok(t) if !t.trim().is_empty() => serde_json::from_str(&t)?,
        _ => json!({}),
    };
    if !root[table].is_object() {
        root[table] = json!({});
    }
    let servers = root[table].as_object_mut().expect("object");
    match entry {
        Some(e) => {
            servers.insert(NAME.into(), e.clone());
        }
        None => {
            servers.remove(NAME);
        }
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = serde_json::to_string_pretty(&root)?;
    text.push('\n');
    write_atomic(file, text.as_bytes())
}

const TOML_HEADER: &str = "[mcp_servers.vway]";

fn toml_block(command: &str) -> String {
    // A JSON string literal is also a valid TOML basic string.
    format!(
        "{TOML_HEADER}\ncommand = {}\nargs = [\"mcp\"]\n",
        json!(command)
    )
}

/// Byte range of the `[mcp_servers.vway]` table, including its sub-tables.
fn toml_range(text: &str) -> Option<(usize, usize)> {
    let mut start = None;
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        match start {
            None if t == TOML_HEADER => start = Some(pos),
            Some(s) if t.starts_with('[') && !t.starts_with("[mcp_servers.vway.") => {
                return Some((s, pos));
            }
            _ => {}
        }
        pos += line.len();
    }
    start.map(|s| (s, text.len()))
}

fn toml_command(block: &str) -> Option<String> {
    block.lines().find_map(|l| {
        let v = l.trim().strip_prefix("command")?.trim().strip_prefix('=')?;
        serde_json::from_str::<String>(v.trim()).ok()
    })
}

fn write_toml_block(file: &Path, block: Option<&str>) -> std::io::Result<()> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut out = match toml_range(&text) {
        Some((s, e)) => {
            let rest = text[e..].trim_start_matches('\n');
            format!("{}{}", &text[..s], rest)
        }
        None => text,
    };
    if block.is_none() && out.ends_with("\n\n") {
        let trimmed = out.trim_end_matches('\n').len();
        out.truncate(trimmed);
        out.push('\n');
    }
    if let Some(b) = block {
        let trimmed = out.trim_end_matches('\n').len();
        out.truncate(trimmed);
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(b);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    write_atomic(file, out.as_bytes())
}

fn run_cli(argv: &[String]) -> Result<(), String> {
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{}: {e}", argv[0]))?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        let msg = msg.trim();
        Err(format!(
            "`{}` failed: {}",
            argv.join(" "),
            if msg.is_empty() { "no message" } else { msg }
        ))
    }
}

fn mcp(ctx: &Ctx, a: Agent, command: &str, mode: Mode, dry_run: bool) -> Option<Item> {
    let want = vec![command.to_string(), "mcp".to_string()];
    let cfg = config(ctx, a, command)?;
    let (target, existing): (String, Result<Option<Vec<String>>, String>) = match &cfg {
        Config::ClaudeUser(f) => (
            "claude mcp (user scope)".into(),
            read_json_entry(f, "mcpServers").map(|e| e.map(|e| entry_argv(&e).unwrap_or_default())),
        ),
        Config::Json { file, table, .. } => (
            file.to_string_lossy().into_owned(),
            read_json_entry(file, table).map(|e| e.map(|e| entry_argv(&e).unwrap_or_default())),
        ),
        Config::Toml(file) => (
            file.to_string_lossy().into_owned(),
            match std::fs::read_to_string(file) {
                Ok(t) => Ok(toml_range(&t).map(|(s, e)| {
                    toml_command(&t[s..e])
                        .map(|c| vec![c, "mcp".into()])
                        .unwrap_or_default()
                })),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.to_string()),
            },
        ),
    };
    let state = match &existing {
        Err(_) => "unreadable",
        Ok(None) => "missing",
        Ok(Some(argv)) if *argv == want => "registered",
        Ok(Some(_)) => "different",
    };
    let mut item = Item {
        kind: "mcp",
        agents: vec![a],
        target,
        state,
        action: None,
        detail: match &existing {
            Err(e) => Some(e.clone()),
            Ok(Some(argv)) if state == "different" => {
                Some(format!("registered as `{}`", argv.join(" ")))
            }
            _ => None,
        },
    };
    let force = matches!(mode, Mode::Setup { force: true, .. });
    let action = match (mode, state) {
        (Mode::Status, _) => return Some(item),
        (Mode::Remove, "missing") => return None,
        (Mode::Remove, "unreadable") | (Mode::Setup { .. }, "unreadable") => {
            item.detail = Some(format!(
                "{}; add it by hand: {}",
                item.detail.take().unwrap_or_default(),
                manual(&cfg, command)
            ));
            "skipped"
        }
        (Mode::Setup { .. }, "registered") => "unchanged",
        (Mode::Setup { .. }, "different") if !force => {
            item.detail = Some(format!(
                "{} (--force replaces it)",
                item.detail.take().unwrap_or_default()
            ));
            "skipped"
        }
        (Mode::Setup { .. }, "different") => "replaced",
        (Mode::Setup { .. }, _) => "created",
        (Mode::Remove, _) => "removed",
    };
    item.action = Some(action);
    if dry_run || matches!(action, "unchanged" | "skipped") {
        return Some(item);
    }
    let add = action != "removed";
    let replace = action == "replaced";
    let result: Result<(), String> = match &cfg {
        Config::ClaudeUser(_) => {
            let Some(claude) = ctx.which("claude") else {
                item.action = Some("skipped");
                item.detail = Some(format!(
                    "`claude` is not on PATH; run: {}",
                    manual(&cfg, command)
                ));
                return Some(item);
            };
            let claude = claude.to_string_lossy().into_owned();
            let remove = || {
                run_cli(&[
                    claude.clone(),
                    "mcp".into(),
                    "remove".into(),
                    "-s".into(),
                    "user".into(),
                    NAME.into(),
                ])
            };
            (|| {
                if !add || replace {
                    remove()?;
                }
                if add {
                    let mut argv: Vec<String> = [&claude, "mcp", "add", "-s", "user", NAME, "--"]
                        .map(String::from)
                        .to_vec();
                    argv.extend(want.iter().cloned());
                    run_cli(&argv)?;
                }
                Ok(())
            })()
        }
        Config::Json { file, table, entry } => {
            write_json_entry(file, table, add.then_some(entry)).map_err(|e| e.to_string())
        }
        Config::Toml(file) => write_toml_block(file, add.then(|| toml_block(command)).as_deref())
            .map_err(|e| e.to_string()),
    };
    if let Err(e) = result {
        item.action = Some("failed");
        item.detail = Some(e);
    }
    Some(item)
}

/// What to add by hand when vway can't edit the file itself.
fn manual(cfg: &Config, command: &str) -> String {
    match cfg {
        Config::ClaudeUser(_) => format!("claude mcp add -s user vway -- {command} mcp"),
        Config::Json { table, entry, .. } => format!("{{\"{table}\": {{\"vway\": {entry}}}}}"),
        Config::Toml(_) => toml_block(command).replace('\n', " "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_table_round_trip() {
        let base = "model = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let dir = std::env::temp_dir().join(format!("vway-toml-{}", std::process::id()));
        let file = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, base).unwrap();

        write_toml_block(&file, Some(&toml_block("/opt/vway"))).unwrap();
        let t = std::fs::read_to_string(&file).unwrap();
        assert!(t.starts_with(base), "{t}");
        let (s, e) = toml_range(&t).unwrap();
        assert_eq!(toml_command(&t[s..e]).as_deref(), Some("/opt/vway"));

        // A later table and a sub-table stay intact / belong to the block.
        std::fs::write(
            &file,
            format!("{t}\n[mcp_servers.vway.env]\nA = \"1\"\n\n[profile]\nx = 1\n"),
        )
        .unwrap();
        write_toml_block(&file, None).unwrap();
        let t = std::fs::read_to_string(&file).unwrap();
        assert_eq!(t, format!("{base}\n[profile]\nx = 1\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn entry_shapes() {
        let a = json!({ "command": "vway", "args": ["mcp"] });
        let b = json!({ "type": "local", "command": ["vway", "mcp"] });
        assert_eq!(entry_argv(&a), entry_argv(&b));
    }

    #[test]
    fn stub_matches_repo_copy() {
        // The repository ships the same stub for plugins and `npx skills add`.
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills/vway/SKILL.md");
        if let Ok(s) = std::fs::read_to_string(repo) {
            assert_eq!(
                s, SKILL,
                "skills/vway/SKILL.md and crates/vway/agent/SKILL.md differ"
            );
        }
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.claude-plugin/plugin.json");
        if let Ok(s) = std::fs::read_to_string(plugin) {
            let v: Value = serde_json::from_str(&s).unwrap();
            assert_eq!(
                v["version"],
                env!("CARGO_PKG_VERSION"),
                "plugin.json version"
            );
        }
    }
}
