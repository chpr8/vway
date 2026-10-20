//! The app environment policy (design 0013).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::GpuMode;

pub type Env = BTreeMap<String, String>;

/// Variables that would let an app reach the host's display server or its IPC.
const HOST_DISPLAY_VARS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_SOCKET",
    "XAUTHORITY",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "SWAYSOCK",
    "I3SOCK",
    "NIRI_SOCKET",
    "DESKTOP_STARTUP_ID",
    "XDG_ACTIVATION_TOKEN",
];

#[derive(Debug, Clone)]
pub enum BusEnv {
    Private(String),
    Host,
    None,
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub runtime_dir: PathBuf,
    pub wayland_display: String,
    /// `Some` for isolated or directory homes.
    pub home: Option<PathBuf>,
    pub bus: BusEnv,
    pub gpu: GpuMode,
    pub mesa_cache: PathBuf,
}

/// The supervisor's own environment (non-UTF-8 entries are dropped).
pub fn process_env() -> Env {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

/// Display variables only; used for helper processes such as the private bus.
pub fn apply_display(env: &mut Env, policy: &Policy) {
    for k in HOST_DISPLAY_VARS {
        env.remove(*k);
    }
    let rt = policy.runtime_dir.to_string_lossy().into_owned();
    env.insert("XDG_RUNTIME_DIR".into(), rt);
    env.insert("WAYLAND_DISPLAY".into(), policy.wayland_display.clone());
    env.insert("XDG_SESSION_TYPE".into(), "wayland".into());
    env.insert("XDG_CURRENT_DESKTOP".into(), "vway".into());
    for k in [
        "GDK_BACKEND",
        "QT_QPA_PLATFORM",
        "SDL_VIDEODRIVER",
        "CLUTTER_BACKEND",
    ] {
        env.insert(k.into(), "wayland".into());
    }
    env.insert("MOZ_ENABLE_WAYLAND".into(), "1".into());
    env.insert("ELECTRON_OZONE_PLATFORM_HINT".into(), "wayland".into());
}

/// Build the app environment: base → policy → overrides (each layer in order). Returns warnings.
pub fn build(
    mut env: Env,
    policy: &Policy,
    overrides: &[&BTreeMap<String, Option<String>>],
) -> (Env, Vec<String>) {
    let mut warnings = Vec::new();
    apply_display(&mut env, policy);

    match &policy.bus {
        BusEnv::Private(addr) => {
            env.insert("DBUS_SESSION_BUS_ADDRESS".into(), addr.clone());
        }
        BusEnv::Host => {
            if env.contains_key("DBUS_SESSION_BUS_ADDRESS") {
                warnings.push(
                    "using the host session bus: single-instance apps may open in an instance \
                     already running on your desktop"
                        .into(),
                );
            }
        }
        BusEnv::None => {
            env.remove("DBUS_SESSION_BUS_ADDRESS");
        }
    }

    if let Some(home) = &policy.home {
        keep_toolchains(&mut env);
        env.insert("HOME".into(), home.to_string_lossy().into_owned());
        for (k, sub) in [
            ("XDG_CONFIG_HOME", ".config"),
            ("XDG_DATA_HOME", ".local/share"),
            ("XDG_CACHE_HOME", ".cache"),
            ("XDG_STATE_HOME", ".local/state"),
        ] {
            env.insert(k.into(), home.join(sub).to_string_lossy().into_owned());
        }
    }

    env.entry("MESA_SHADER_CACHE_DIR".into())
        .or_insert_with(|| policy.mesa_cache.to_string_lossy().into_owned());

    if policy.gpu == GpuMode::Software {
        match find_lavapipe_icd(&env) {
            Some(icd) => {
                env.insert("VK_DRIVER_FILES".into(), icd.to_string_lossy().into_owned());
                env.remove("VK_ICD_FILENAMES");
            }
            None => warnings.push(
                "--gpu software: no lavapipe Vulkan ICD (lvp_icd*.json) found; Vulkan apps such \
                 as GPUI will fail with \"no GPU adapters\". Install Mesa's Vulkan software \
                 driver (e.g. vulkan-swrast on Arch, mesa-vulkan-drivers on Debian/Ubuntu)"
                    .into(),
            ),
        }
        env.insert("LIBGL_ALWAYS_SOFTWARE".into(), "1".into());
        env.insert("GALLIUM_DRIVER".into(), "llvmpipe".into());
        env.insert("__GLX_VENDOR_LIBRARY_NAME".into(), "mesa".into());
        if let Some(vendor) = find_mesa_egl_vendor() {
            env.insert(
                "__EGL_VENDOR_LIBRARY_FILENAMES".into(),
                vendor.to_string_lossy().into_owned(),
            );
        }
    }

    for layer in overrides {
        for (k, v) in layer.iter() {
            match v {
                Some(v) => env.insert(k.clone(), v.clone()),
                None => env.remove(k),
            };
        }
    }
    (env, warnings)
}

/// Point toolchain managers at their real homes before `HOME` and the XDG directories move, so
/// `cargo run` in a session uses the installed toolchain instead of downloading a new one into the
/// session's home. Only directories that exist are kept, and explicit settings win.
fn keep_toolchains(env: &mut Env) {
    let Some(home) = env.get("HOME").filter(|h| !h.is_empty()).map(PathBuf::from) else {
        return;
    };
    let xdg = |k: &str, default: &str| {
        env.get(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(default))
    };
    let dirs = [
        ("CARGO_HOME", home.join(".cargo")),
        ("RUSTUP_HOME", home.join(".rustup")),
        (
            "MISE_DATA_DIR",
            xdg("XDG_DATA_HOME", ".local/share").join("mise"),
        ),
        (
            "MISE_CONFIG_DIR",
            xdg("XDG_CONFIG_HOME", ".config").join("mise"),
        ),
        (
            "MISE_STATE_DIR",
            xdg("XDG_STATE_HOME", ".local/state").join("mise"),
        ),
        (
            "MISE_CACHE_DIR",
            xdg("XDG_CACHE_HOME", ".cache").join("mise"),
        ),
    ];
    for (k, dir) in dirs {
        if !env.contains_key(k) && dir.is_dir() {
            env.insert(k.into(), dir.to_string_lossy().into_owned());
        }
    }
}

/// Directories the Vulkan loader searches for `vulkan/icd.d`.
fn vulkan_icd_dirs(env: &Env) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let split = |v: Option<&String>, default: &str| -> Vec<PathBuf> {
        v.map(String::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect()
    };
    roots.extend(split(env.get("XDG_CONFIG_DIRS"), "/etc/xdg"));
    roots.push("/etc".into());
    roots.extend(split(
        env.get("XDG_DATA_DIRS"),
        "/usr/local/share:/usr/share",
    ));
    roots.push("/usr/share".into());
    roots.into_iter().map(|r| r.join("vulkan/icd.d")).collect()
}

pub fn find_lavapipe_icd(env: &Env) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in vulkan_icd_dirs(env) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut here: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("lvp_icd") && n.ends_with(".json"))
            })
            .collect();
        here.sort();
        found.extend(here);
    }
    // Prefer the native architecture's ICD.
    let arch = std::env::consts::ARCH;
    found
        .iter()
        .find(|p| p.to_string_lossy().contains(arch))
        .or(found.first())
        .cloned()
}

pub fn find_mesa_egl_vendor() -> Option<PathBuf> {
    ["/usr/share/glvnd/egl_vendor.d", "/etc/glvnd/egl_vendor.d"]
        .iter()
        .map(|d| Path::new(d).join("50_mesa.json"))
        .find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy {
            runtime_dir: "/s/xdg".into(),
            wayland_display: "wayland-0".into(),
            home: Some("/s/home".into()),
            bus: BusEnv::None,
            gpu: GpuMode::Auto,
            mesa_cache: "/r/cache/mesa".into(),
        }
    }

    #[test]
    fn layers_apply_in_order() {
        let base: Env = [
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/bus"),
            ("PATH", "/bin"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let session: BTreeMap<String, Option<String>> =
            [("LANG".to_string(), Some("C".to_string()))].into();
        let launch: BTreeMap<String, Option<String>> = [
            ("LANG".to_string(), Some("C.UTF-8".to_string())),
            ("PATH".to_string(), None),
        ]
        .into();
        let (env, warnings) = build(base, &policy(), &[&session, &launch]);
        assert!(warnings.is_empty());
        assert_eq!(env.get("DISPLAY"), None);
        assert_eq!(env["WAYLAND_DISPLAY"], "wayland-0");
        assert_eq!(env["XDG_RUNTIME_DIR"], "/s/xdg");
        assert_eq!(env.get("DBUS_SESSION_BUS_ADDRESS"), None);
        assert_eq!(env["HOME"], "/s/home");
        assert_eq!(env["XDG_CONFIG_HOME"], "/s/home/.config");
        assert_eq!(env["MESA_SHADER_CACHE_DIR"], "/r/cache/mesa");
        assert_eq!(env["LANG"], "C.UTF-8");
        assert_eq!(env.get("PATH"), None);
    }

    #[test]
    fn toolchains_survive_the_isolated_home() {
        let real = std::env::temp_dir().join(format!("vway-env-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&real);
        for d in [".cargo", ".rustup", "data/mise"] {
            std::fs::create_dir_all(real.join(d)).unwrap();
        }
        let s = |p: PathBuf| p.to_string_lossy().into_owned();
        let base: Env = [
            ("HOME".to_string(), s(real.clone())),
            ("XDG_DATA_HOME".to_string(), s(real.join("data"))),
            ("RUSTUP_HOME".to_string(), "/opt/rustup".to_string()),
        ]
        .into();
        let (env, _) = build(base, &policy(), &[]);
        assert_eq!(env["HOME"], "/s/home");
        assert_eq!(env["CARGO_HOME"], s(real.join(".cargo")));
        assert_eq!(env["RUSTUP_HOME"], "/opt/rustup", "explicit settings win");
        assert_eq!(env["MISE_DATA_DIR"], s(real.join("data/mise")));
        assert_eq!(
            env.get("MISE_CONFIG_DIR"),
            None,
            "missing directories are not invented"
        );
        let _ = std::fs::remove_dir_all(&real);
    }
}
