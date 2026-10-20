//! `vway doctor`: what the host has, what is missing, and the package that provides it.
//!
//! vway never installs system packages; it prints the command for the detected distribution.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::agents;
use crate::session;

/// Packages vway or the apps it runs may need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pkg {
    XkbData,
    Dbus,
    VulkanLoader,
    Lavapipe,
    MesaEgl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Arch,
    Debian,
    Fedora,
    Suse,
    Alpine,
}

impl Family {
    /// From `/etc/os-release`: `ID`, then each `ID_LIKE` entry.
    fn detect(os_release: &str) -> Option<(String, Family)> {
        let field = |k: &str| {
            os_release.lines().find_map(|l| {
                l.strip_prefix(k)?
                    .strip_prefix('=')
                    .map(|v| v.trim().trim_matches('"').to_string())
            })
        };
        let id = field("ID").unwrap_or_default();
        let like = field("ID_LIKE").unwrap_or_default();
        let family = std::iter::once(id.as_str())
            .chain(like.split_whitespace())
            .find_map(|i| {
                Some(match i {
                    "arch" => Family::Arch,
                    "debian" | "ubuntu" => Family::Debian,
                    "fedora" | "rhel" | "centos" => Family::Fedora,
                    "suse" | "opensuse" => Family::Suse,
                    "alpine" => Family::Alpine,
                    _ => return None,
                })
            })?;
        Some((id, family))
    }

    fn install(self) -> &'static str {
        match self {
            Family::Arch => "sudo pacman -S --needed",
            Family::Debian => "sudo apt install",
            Family::Fedora => "sudo dnf install",
            Family::Suse => "sudo zypper install",
            Family::Alpine => "sudo apk add",
        }
    }

    fn package(self, p: Pkg) -> &'static str {
        use Family::*;
        use Pkg::*;
        match (self, p) {
            (Debian, XkbData) => "xkb-data",
            (_, XkbData) => "xkeyboard-config",
            (Debian | Fedora, Dbus) => "dbus-daemon",
            (Suse, Dbus) => "dbus-1",
            (_, Dbus) => "dbus",
            (Arch, VulkanLoader) => "vulkan-icd-loader",
            (Debian | Suse, VulkanLoader) => "libvulkan1",
            (_, VulkanLoader) => "vulkan-loader",
            (Arch, Lavapipe) => "vulkan-swrast",
            (Debian | Fedora, Lavapipe) => "mesa-vulkan-drivers",
            (Suse, Lavapipe) => "libvulkan_lvp",
            (Alpine, Lavapipe) => "mesa-vulkan-swrast",
            (Arch, MesaEgl) => "mesa",
            (Debian, MesaEgl) => "libegl-mesa0",
            (Fedora, MesaEgl) => "mesa-libEGL",
            (Suse, MesaEgl) => "Mesa-libEGL1",
            (Alpine, MesaEgl) => "mesa-egl",
        }
    }
}

fn generic(p: Pkg) -> &'static str {
    match p {
        Pkg::XkbData => "xkeyboard-config (XKB keymap data)",
        Pkg::Dbus => "dbus-daemon",
        Pkg::VulkanLoader => "the Vulkan loader (libvulkan.so.1)",
        Pkg::Lavapipe => "Mesa's lavapipe Vulkan driver",
        Pkg::MesaEgl => "Mesa's EGL driver",
    }
}

struct Checks {
    family: Option<(String, Family)>,
    list: Vec<Value>,
    missing: Vec<Pkg>,
    ok: bool,
}

impl Checks {
    fn add(&mut self, name: &str, required: bool, pass: bool, detail: String, pkg: Option<Pkg>) {
        let mut c = json!({ "name": name, "ok": pass, "required": required, "detail": detail });
        if !pass {
            self.ok &= !required;
            if let Some(p) = pkg {
                c["fix"] = json!(match &self.family {
                    Some((_, f)) => format!("{} {}", f.install(), f.package(p)),
                    None => format!("install {}", generic(p)),
                });
                self.missing.push(p);
            }
        }
        self.list.push(c);
    }
}

/// Library directories the dynamic loader searches by default, plus `LD_LIBRARY_PATH`.
fn lib_dirs() -> Vec<PathBuf> {
    let triple = format!("{}-linux-gnu", std::env::consts::ARCH);
    let mut dirs: Vec<PathBuf> = std::env::var_os("LD_LIBRARY_PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for d in ["/usr/lib", "/usr/lib64", "/lib", "/lib64", "/usr/local/lib"] {
        dirs.push(d.into());
    }
    dirs.push(Path::new("/usr/lib").join(&triple));
    dirs.push(Path::new("/lib").join(&triple));
    dirs
}

/// Run every check. Returns the document and whether all required checks passed.
pub fn run() -> (Value, bool) {
    let family = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|s| Family::detect(&s));
    let mut c = Checks {
        family,
        list: Vec::new(),
        missing: Vec::new(),
        ok: true,
    };

    match session::state_root() {
        Ok(root) => {
            let len = root.as_os_str().len() + "/sessions/".len() + 32 + "/xdg/wayland-0".len();
            c.add("state_root", true, true, root.display().to_string(), None);
            c.add(
                "socket_path_length",
                true,
                len <= 107,
                format!("longest socket path ≈{len} bytes (limit 107)"),
                None,
            );
        }
        Err(e) => c.add("state_root", true, false, e.message, None),
    }

    let path: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let loc = agents::locate(&path);
    let mut detail = match &loc.warning {
        Some(w) => w.clone(),
        None => format!("`vway` on PATH runs {}", loc.exe.display()),
    };
    let mut real: Vec<PathBuf> = loc
        .on_path
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();
    real.dedup();
    if real.len() > 1 {
        let shadowed: Vec<String> = loc.on_path[1..]
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        detail.push_str(&format!(
            "; other copies later on PATH are shadowed: {}",
            shadowed.join(", ")
        ));
    }
    c.add("vway_on_path", false, loc.path_runs_this, detail, None);

    match vway_compositor::keymap::ReverseKeymap::new("us", "") {
        Ok(_) => c.add("xkb_keymap", true, true, "us layout compiles".into(), None),
        Err(e) => c.add("xkb_keymap", true, false, e, Some(Pkg::XkbData)),
    }

    match agents::which_in(&path, "dbus-daemon") {
        Some(p) => c.add("dbus_daemon", false, true, p.display().to_string(), None),
        None => c.add(
            "dbus_daemon",
            false,
            false,
            "not found: sessions fall back to no private bus (--dbus none/host)".into(),
            Some(Pkg::Dbus),
        ),
    }

    let nodes: Vec<String> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("renderD"))
        .collect();
    c.add(
        "gpu_render_node",
        false,
        !nodes.is_empty(),
        if nodes.is_empty() {
            "no /dev/dri/renderD*: apps render in software (use --gpu software)".into()
        } else {
            format!("/dev/dri/{}", nodes.join(", /dev/dri/"))
        },
        None,
    );

    match lib_dirs()
        .into_iter()
        .map(|d| d.join("libvulkan.so.1"))
        .find(|p| p.exists())
    {
        Some(p) => c.add("vulkan_loader", false, true, p.display().to_string(), None),
        None => c.add(
            "vulkan_loader",
            false,
            false,
            "no libvulkan.so.1: Vulkan apps such as GPUI cannot start".into(),
            Some(Pkg::VulkanLoader),
        ),
    }

    let env = vway_compositor::env::process_env();
    match vway_compositor::env::find_lavapipe_icd(&env) {
        Some(p) => c.add("lavapipe_icd", false, true, p.display().to_string(), None),
        None => c.add(
            "lavapipe_icd",
            false,
            false,
            "no lavapipe Vulkan ICD: --gpu software cannot serve Vulkan apps (e.g. GPUI)".into(),
            Some(Pkg::Lavapipe),
        ),
    }
    match vway_compositor::env::find_mesa_egl_vendor() {
        Some(p) => c.add("mesa_egl", false, true, p.display().to_string(), None),
        None => c.add(
            "mesa_egl",
            false,
            false,
            "no Mesa GLVND EGL vendor file: vway composites with pixman (CPU)".into(),
            Some(Pkg::MesaEgl),
        ),
    }

    match agents::Ctx::from_env(None) {
        Ok(ctx) => {
            let report = agents::run(&ctx, agents::Mode::Status, &[], None, false);
            let skills: Vec<String> = report.value["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|i| i["kind"] == "skill")
                .map(|i| {
                    let s = |k: &str| i[k].as_str().unwrap_or("?").to_string();
                    format!("{}: {}", s("target"), s("state"))
                })
                .collect();
            let installed = report.value["items"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|i| i["kind"] == "skill" && i["state"] == "current");
            let mut check = json!({
                "name": "agent_skill",
                "ok": installed,
                "required": false,
                "detail": skills.join("; "),
            });
            if !installed {
                check["fix"] = json!("vway agents setup");
            }
            c.list.push(check);
        }
        Err(e) => c.add("agent_skill", false, false, e.message, None),
    }

    let mut v = json!({ "checks": c.list });
    if let Some((id, _)) = &c.family {
        v["distro"] = json!(id);
    }
    c.missing.dedup();
    if let (Some((_, f)), false) = (&c.family, c.missing.is_empty()) {
        let pkgs: Vec<&str> = c.missing.iter().map(|p| f.package(*p)).collect();
        v["install"] = json!(format!("{} {}", f.install(), pkgs.join(" ")));
    }
    (v, c.ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distro_families() {
        let fam = |s: &str| Family::detect(s).map(|(_, f)| f);
        assert_eq!(fam("ID=omarchy\nID_LIKE=arch\n"), Some(Family::Arch));
        assert_eq!(
            fam("ID=pop\nID_LIKE=\"ubuntu debian\"\n"),
            Some(Family::Debian)
        );
        assert_eq!(
            fam("ID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\"\n"),
            Some(Family::Suse)
        );
        assert_eq!(fam("ID=fedora\n"), Some(Family::Fedora));
        assert_eq!(fam("ID=nixos\n"), None);
        assert_eq!(Family::Debian.package(Pkg::Lavapipe), "mesa-vulkan-drivers");
    }
}
