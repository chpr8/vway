//! `vway agents` and `vway guide` against a throwaway home. Agent CLIs are stubs that log their
//! arguments, so nothing here touches the real agent configuration.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use common::*;
use serde_json::{Value, json};

static NEXT: AtomicU32 = AtomicU32::new(0);

struct Home {
    root: PathBuf,
}

impl Home {
    /// Under the target directory, so tests can hard-link the binary.
    fn new() -> Home {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "agents-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "bin", "project"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Home { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    /// A stub agent CLI that appends its arguments to `calls.log`.
    fn stub(&self, name: &str) {
        let p = self.root.join("bin").join(name);
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\necho \"{name} $*\" >> {}/calls.log\n",
                self.root.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn cmd_with(&self, exe: &Path, path: &[PathBuf], args: &[&str]) -> Command {
        let mut c = Command::new(exe);
        c.env_clear()
            .env("HOME", self.home())
            .env("PATH", std::env::join_paths(path).unwrap())
            .current_dir(self.project())
            .args(args);
        c
    }

    fn run(&self, args: &[&str]) -> Out {
        let path = [self.root.join("bin")];
        let mut c = self.cmd_with(&vway_bin(), &path, args);
        Harness::parse(&mut c)
    }

    fn ok(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert_eq!(out.code, 0, "vway {args:?}: {}\n{}", out.json, out.stderr);
        out.json
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `items` keyed by `kind:target-suffix`, value = `(state, action)`.
fn item<'a>(v: &'a Value, kind: &str, target_suffix: &str) -> &'a Value {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == kind && i["target"].as_str().unwrap().ends_with(target_suffix))
        .unwrap_or_else(|| panic!("no {kind} item ending in {target_suffix}: {v:#}"))
}

fn actions(v: &Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["action"].as_str().unwrap_or("-").to_string())
        .collect()
}

fn installed_skill() -> String {
    let out = Command::new(vway_bin())
        .args(["guide", "--skill"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn guide_prints_markdown() {
    let out = Command::new(vway_bin()).arg("guide").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(s.starts_with("# vway guide"), "{s}");
    let skill = installed_skill();
    assert!(skill.starts_with("---\nname: vway\n"), "{skill}");
    assert!(skill.contains("vway guide"));
}

#[test]
fn setup_installs_skills_for_detected_agents_and_is_idempotent() {
    let h = Home::new();
    std::fs::create_dir_all(h.home().join(".claude")).unwrap();
    std::fs::create_dir_all(h.home().join(".config/opencode")).unwrap();

    let v = h.ok(&["agents", "setup"]);
    assert_eq!(v["detected"], json!(["claude", "opencode"]), "{v:#}");
    assert!(v.get("mcp_command").is_none(), "no MCP without --mcp");
    assert_eq!(actions(&v), ["created", "created"], "{v:#}");
    let shared = item(&v, "skill", ".agents/skills/vway/SKILL.md");
    assert_eq!(shared["agents"], json!(["opencode"]));
    let stub = installed_skill();
    for rel in [
        "home/.agents/skills/vway/SKILL.md",
        "home/.claude/skills/vway/SKILL.md",
    ] {
        let s = h.read(rel);
        assert!(s.starts_with(&stub), "{rel}");
        assert!(s.contains("installed by `vway agents setup`"), "{rel}");
    }

    let v = h.ok(&["agents", "setup"]);
    assert_eq!(actions(&v), ["unchanged", "unchanged"], "{v:#}");
    let v = h.ok(&["agents", "status"]);
    assert_eq!(
        item(&v, "skill", ".claude/skills/vway/SKILL.md")["state"],
        "current"
    );
    assert!(
        item(&v, "skill", ".claude/skills/vway/SKILL.md")
            .get("action")
            .is_none()
    );

    // An older stub (still carrying the marker) is updated in place.
    let claude = h.home().join(".claude/skills/vway/SKILL.md");
    let old = std::fs::read_to_string(&claude)
        .unwrap()
        .replace("Run `vway guide`", "Old text");
    std::fs::write(&claude, old).unwrap();
    let v = h.ok(&["agents", "setup"]);
    let it = item(&v, "skill", ".claude/skills/vway/SKILL.md");
    assert_eq!(
        (&it["state"], &it["action"]),
        (&json!("outdated"), &json!("updated"))
    );
}

#[test]
fn claude_config_dir_is_respected() {
    let h = Home::new();
    let dir = h.home().join(".config/claude");
    std::fs::create_dir_all(&dir).unwrap();

    let path = [h.root.join("bin")];
    let mut c = h.cmd_with(&vway_bin(), &path, &["agents", "setup"]);
    c.env("CLAUDE_CONFIG_DIR", &dir);
    let v = Harness::parse(&mut c).json;
    assert_eq!(v["detected"], json!(["claude"]), "{v:#}");
    item(&v, "skill", ".config/claude/skills/vway/SKILL.md");
    assert!(
        h.read("home/.config/claude/skills/vway/SKILL.md")
            .starts_with(&installed_skill())
    );
    assert!(!h.home().join(".claude").exists());
}

#[test]
fn foreign_skills_are_left_alone_unless_forced() {
    let h = Home::new();
    let dir = h.home().join(".agents/skills/vway");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), "my own vway notes\n").unwrap();

    let v = h.ok(&["agents", "setup", "--agent", "codex"]);
    let it = item(&v, "skill", ".agents/skills/vway/SKILL.md");
    assert_eq!(
        (&it["state"], &it["action"]),
        (&json!("foreign"), &json!("skipped"))
    );
    assert!(
        v["warnings"][0].as_str().unwrap().contains("--force"),
        "{v:#}"
    );
    assert_eq!(
        h.read("home/.agents/skills/vway/SKILL.md"),
        "my own vway notes\n"
    );

    let v = h.ok(&["agents", "remove", "--agent", "codex"]);
    assert_eq!(
        item(&v, "skill", ".agents/skills/vway/SKILL.md")["action"],
        "skipped"
    );
    assert!(dir.join("SKILL.md").exists());

    let v = h.ok(&["agents", "setup", "--agent", "codex", "--force"]);
    assert_eq!(
        item(&v, "skill", ".agents/skills/vway/SKILL.md")["action"],
        "replaced"
    );
    assert!(
        h.read("home/.agents/skills/vway/SKILL.md")
            .starts_with(&installed_skill())
    );
}

#[test]
fn mcp_registration_for_every_agent() {
    let h = Home::new();
    h.stub("claude");
    // Existing configuration must survive.
    std::fs::create_dir_all(h.home().join(".config/opencode")).unwrap();
    std::fs::write(
        h.home().join(".config/opencode/opencode.json"),
        r#"{ "theme": "dark", "mcp": { "other": { "type": "local", "command": ["x"] } } }"#,
    )
    .unwrap();
    std::fs::create_dir_all(h.home().join(".codex")).unwrap();
    std::fs::write(h.home().join(".codex/config.toml"), "model = \"o3\"\n").unwrap();
    std::fs::create_dir_all(h.home().join(".gemini")).unwrap();
    std::fs::write(h.home().join(".gemini/settings.json"), "{ // comment\n}\n").unwrap();

    let all = "claude,codex,gemini,cursor,opencode";
    let v = h.ok(&[
        "agents",
        "setup",
        "--mcp",
        "--agent",
        all,
        "--command",
        "vway",
    ]);
    assert_eq!(v["mcp_command"], json!(["vway", "mcp"]));
    assert_eq!(
        h.calls(),
        ["claude mcp add -s user vway -- vway mcp"],
        "{v:#}"
    );
    assert_eq!(item(&v, "mcp", "(user scope)")["action"], "created");

    let toml = h.read("home/.codex/config.toml");
    assert_eq!(
        toml,
        "model = \"o3\"\n\n[mcp_servers.vway]\ncommand = \"vway\"\nargs = [\"mcp\"]\n"
    );
    let oc: Value = serde_json::from_str(&h.read("home/.config/opencode/opencode.json")).unwrap();
    assert_eq!(oc["theme"], "dark");
    assert_eq!(oc["mcp"]["other"]["command"], json!(["x"]));
    assert_eq!(oc["mcp"]["vway"]["command"], json!(["vway", "mcp"]));
    let cursor: Value = serde_json::from_str(&h.read("home/.cursor/mcp.json")).unwrap();
    assert_eq!(
        cursor["mcpServers"]["vway"],
        json!({ "command": "vway", "args": ["mcp"] })
    );
    // Files with comments are not rewritten; the item says what to add by hand.
    let g = item(&v, "mcp", ".gemini/settings.json");
    assert_eq!(
        (&g["state"], &g["action"]),
        (&json!("unreadable"), &json!("skipped"))
    );
    assert!(
        g["detail"].as_str().unwrap().contains("add it by hand"),
        "{g}"
    );
    assert_eq!(h.read("home/.gemini/settings.json"), "{ // comment\n}\n");

    // What the real `claude mcp add` leaves behind; vway only ever reads this file.
    std::fs::write(
        h.home().join(".claude.json"),
        r#"{"mcpServers":{"vway":{"type":"stdio","command":"vway","args":["mcp"],"env":{}}}}"#,
    )
    .unwrap();

    // Second run: nothing changes.
    let v = h.ok(&[
        "agents",
        "setup",
        "--mcp",
        "--agent",
        "claude,codex,cursor,opencode",
        "--command",
        "vway",
    ]);
    assert_eq!(h.calls().len(), 1, "claude was called again");
    for suffix in ["(user scope)", "config.toml", "mcp.json", "opencode.json"] {
        assert_eq!(
            item(&v, "mcp", suffix)["action"],
            "unchanged",
            "{suffix}: {v:#}"
        );
    }
    // A different command is reported, and only replaced with --force.
    let v = h.ok(&[
        "agents",
        "setup",
        "--mcp",
        "--agent",
        "cursor",
        "--command",
        "/opt/vway",
    ]);
    let c = item(&v, "mcp", "mcp.json");
    assert_eq!(
        (&c["state"], &c["action"]),
        (&json!("different"), &json!("skipped"))
    );
    let v = h.ok(&[
        "agents",
        "setup",
        "--mcp",
        "--agent",
        "cursor",
        "--command",
        "/opt/vway",
        "--force",
    ]);
    assert_eq!(item(&v, "mcp", "mcp.json")["action"], "replaced");

    // Remove takes out exactly what setup added.
    let v = h.ok(&[
        "agents",
        "remove",
        "--agent",
        "claude,codex,cursor,opencode",
    ]);
    assert!(
        h.calls()
            .contains(&"claude mcp remove -s user vway".to_string())
    );
    assert_eq!(h.read("home/.codex/config.toml"), "model = \"o3\"\n");
    let oc: Value = serde_json::from_str(&h.read("home/.config/opencode/opencode.json")).unwrap();
    assert!(oc["mcp"].get("vway").is_none());
    assert_eq!(oc["mcp"]["other"]["command"], json!(["x"]));
    assert_eq!(
        item(&v, "skill", ".agents/skills/vway/SKILL.md")["action"],
        "removed"
    );
    assert!(!h.home().join(".agents/skills/vway").exists());
}

#[test]
fn project_scope_and_dry_run() {
    let h = Home::new();
    let v = h.ok(&[
        "agents",
        "setup",
        "--project",
        "--mcp",
        "--agent",
        "claude,opencode",
        "--command",
        "vway",
        "--dry-run",
    ]);
    assert_eq!(v["scope"], "project");
    assert_eq!(v["dry_run"], true);
    assert_eq!(
        actions(&v),
        ["created", "created", "created", "created"],
        "{v:#}"
    );
    assert!(
        std::fs::read_dir(h.project()).unwrap().next().is_none(),
        "dry run wrote files"
    );

    std::fs::write(h.project().join("opencode.jsonc"), "{}\n").unwrap();
    let v = h.ok(&[
        "agents",
        "setup",
        "--project",
        "--mcp",
        "--agent",
        "claude,opencode",
        "--command",
        "vway",
    ]);
    assert!(
        h.calls().is_empty(),
        "project scope edits .mcp.json directly"
    );
    let mcp: Value = serde_json::from_str(&h.read("project/.mcp.json")).unwrap();
    assert_eq!(
        mcp["mcpServers"]["vway"],
        json!({ "type": "stdio", "command": "vway", "args": ["mcp"] })
    );
    assert!(h.project().join(".claude/skills/vway/SKILL.md").exists());
    assert!(h.project().join(".agents/skills/vway/SKILL.md").exists());
    let oc = item(&v, "mcp", "opencode.json");
    assert_eq!(oc["action"], "skipped");
    assert!(
        oc["detail"].as_str().unwrap().contains("opencode.jsonc"),
        "{oc}"
    );
}

#[test]
fn mcp_command_follows_path() {
    let h = Home::new();
    // Not on PATH: the absolute path, with a warning.
    let v = h.ok(&["agents", "setup", "--mcp", "--agent", "cursor", "--dry-run"]);
    let exe = vway_bin().canonicalize().unwrap();
    assert_eq!(v["mcp_command"][0], exe.to_str().unwrap());
    assert!(v["warnings"][0].as_str().unwrap().contains("not on PATH"));

    // On PATH (a symlink to this binary): the bare name.
    std::os::unix::fs::symlink(&exe, h.root.join("bin/vway")).unwrap();
    let v = h.ok(&["agents", "setup", "--mcp", "--agent", "cursor", "--dry-run"]);
    assert_eq!(v["mcp_command"][0], "vway", "{v:#}");
    assert!(v.get("warnings").is_none(), "{v:#}");

    // A mise install: agents get the shim, which survives upgrades and needs no `mise activate`.
    let data = h.root.join("mise");
    let bin = data.join("installs/vway/1.2.3/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(data.join("shims")).unwrap();
    std::fs::hard_link(&exe, bin.join("vway")).unwrap();
    let mise = h.root.join("bin/mise");
    std::fs::write(&mise, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&mise, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&mise, data.join("shims/vway")).unwrap();
    let path = [data.join("shims")];
    let mut c = h.cmd_with(
        &bin.join("vway"),
        &path,
        &["agents", "setup", "--mcp", "--agent", "cursor", "--dry-run"],
    );
    let out = Harness::parse(&mut c);
    assert_eq!(
        out.json["mcp_command"][0],
        data.join("shims/vway").to_str().unwrap(),
        "{:#}",
        out.json
    );
}

#[test]
fn doctor_reports_path_and_skill() {
    let h = Home::new();
    let check = |v: &Value, name: &str| -> Value {
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name} check: {v:#}"))
    };
    let doctor = || {
        let mut c = h.cmd_with(&vway_bin(), &[h.root.join("bin")], &["doctor"]);
        c.env("VWAY_STATE_DIR", h.root.join("state"));
        Harness::parse(&mut c).json
    };
    let v = doctor();
    assert_eq!(check(&v, "vway_on_path")["ok"], false);
    assert_eq!(check(&v, "agent_skill")["fix"], "vway agents setup");

    let exe = vway_bin().canonicalize().unwrap();
    std::os::unix::fs::symlink(&exe, h.root.join("bin/vway")).unwrap();
    h.ok(&["agents", "setup", "--agent", "codex"]);
    let v = doctor();
    assert_eq!(check(&v, "vway_on_path")["ok"], true, "{v:#}");
    assert_eq!(check(&v, "agent_skill")["ok"], true, "{v:#}");
    // Missing optional pieces come with an install command when the distro is known.
    for c in v["checks"].as_array().unwrap() {
        if c["ok"] == false && c.get("fix").is_some() && v.get("distro").is_some() {
            assert!(v["install"].is_string(), "{v:#}");
        }
    }
}
