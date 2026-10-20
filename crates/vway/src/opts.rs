//! Session options shared by the CLI (`clap`) and the MCP server (`serde`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use vway_compositor::SessionConfig;
use vway_compositor::config::{DbusMode, GpuMode, HomeMode, WindowMode};
use vway_proto::{ErrorBody, ErrorCode, err};

#[derive(Debug, Clone, Default, clap::Args, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionOpts {
    /// Logical output size, WxH [default: 1280x800]
    #[arg(long, value_name = "WxH")]
    pub resolution: Option<String>,
    /// Output scale (multiples of 1/120, e.g. 1, 1.25, 2) [default: 1]
    #[arg(long)]
    pub scale: Option<f64>,
    /// Phone-like output: 390x844 at scale 3, fullscreen windows
    #[arg(long)]
    pub mobile: bool,
    /// maximized | fullscreen | floating [default: maximized]
    #[arg(long, value_name = "MODE")]
    pub window_mode: Option<String>,
    /// auto | software [default: auto]
    #[arg(long, value_name = "MODE")]
    pub gpu: Option<String>,
    /// isolated | host | <dir> [default: isolated]
    #[arg(long, value_name = "MODE")]
    pub home: Option<String>,
    /// private | host | none [default: private]
    #[arg(long, value_name = "MODE")]
    pub dbus: Option<String>,
    /// xkb layout and optional variant, LAYOUT[:VARIANT] [default: us]
    #[arg(long, value_name = "LAYOUT[:VARIANT]")]
    pub keymap: Option<String>,
    /// End the session when the app exits
    #[arg(long)]
    pub exit_with_app: bool,
    /// Session-wide environment override for launched apps (K=V sets, K= unsets); repeatable
    #[arg(long = "env", value_name = "K=V")]
    #[serde(skip)]
    pub env: Vec<String>,
}

impl SessionOpts {
    pub fn to_config(&self, state_root: &Path) -> Result<SessionConfig, ErrorBody> {
        let usage = |m: String| err(ErrorCode::Usage, m);
        let mut cfg = SessionConfig::new("", state_root.to_path_buf());
        if self.mobile {
            cfg.width = 390;
            cfg.height = 844;
            cfg.scale = 3.0;
            cfg.window_mode = WindowMode::Fullscreen;
        }
        if let Some(r) = &self.resolution {
            let (w, h) = parse_size(r).map_err(usage)?;
            cfg.width = w;
            cfg.height = h;
        }
        if let Some(s) = self.scale {
            cfg.scale = s;
        }
        if let Some(m) = &self.window_mode {
            cfg.window_mode = match m.as_str() {
                "maximized" => WindowMode::Maximized,
                "fullscreen" => WindowMode::Fullscreen,
                "floating" => WindowMode::Floating,
                _ => {
                    return Err(usage(format!(
                        "--window-mode {m:?}: use maximized, fullscreen or floating"
                    )));
                }
            };
        }
        if let Some(g) = &self.gpu {
            cfg.gpu = match g.as_str() {
                "auto" => GpuMode::Auto,
                "software" => GpuMode::Software,
                _ => return Err(usage(format!("--gpu {g:?}: use auto or software"))),
            };
        }
        if let Some(h) = &self.home {
            cfg.home = match h.as_str() {
                "isolated" => HomeMode::Isolated,
                "host" => HomeMode::Host,
                "" => return Err(usage("--home needs isolated, host or a directory".into())),
                dir => HomeMode::Dir(crate::actions::absolute(&PathBuf::from(dir))),
            };
        }
        if let Some(d) = &self.dbus {
            cfg.dbus = match d.as_str() {
                "private" => DbusMode::Private,
                "host" => DbusMode::Host,
                "none" => DbusMode::None,
                _ => return Err(usage(format!("--dbus {d:?}: use private, host or none"))),
            };
        }
        if let Some(k) = &self.keymap {
            let (layout, variant) = k.split_once(':').unwrap_or((k, ""));
            if layout.is_empty() {
                return Err(usage(
                    "--keymap needs a layout, e.g. us or de:nodeadkeys".into(),
                ));
            }
            cfg.keymap_layout = layout.into();
            cfg.keymap_variant = variant.into();
        }
        cfg.exit_with_app = self.exit_with_app;
        cfg.env = parse_env(&self.env)?;
        cfg.validate().map_err(usage)?;
        Ok(cfg)
    }
}

pub fn parse_size(s: &str) -> Result<(i32, i32), String> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("{s:?} is not WxH"))?;
    let p = |v: &str| {
        v.trim()
            .parse::<i32>()
            .map_err(|_| format!("{s:?} is not WxH"))
    };
    Ok((p(w)?, p(h)?))
}

/// `K=V` sets, `K=` unsets (so a host variable can be removed for one launch).
pub fn parse_env(items: &[String]) -> Result<BTreeMap<String, Option<String>>, ErrorBody> {
    let mut env = BTreeMap::new();
    for item in items {
        let (k, v) = item
            .split_once('=')
            .ok_or_else(|| err(ErrorCode::Usage, format!("--env {item:?} is not K=V")))?;
        if k.is_empty() || k.contains('\0') || v.contains('\0') {
            return Err(err(ErrorCode::Usage, format!("--env {item:?} is not K=V")));
        }
        env.insert(k.to_string(), (!v.is_empty()).then(|| v.to_string()));
    }
    Ok(env)
}

/// Parse `X,Y` (logical pixels).
pub fn parse_point(s: &str) -> Result<(f64, f64), String> {
    let v = parse_floats(s, 2)?;
    Ok((v[0], v[1]))
}

pub fn parse_floats(s: &str, n: usize) -> Result<Vec<f64>, String> {
    let v: Vec<f64> = s
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("{s:?}: expected {n} comma-separated numbers"))?;
    if v.len() != n || !v.iter().all(|f| f.is_finite()) {
        return Err(format!("{s:?}: expected {n} comma-separated numbers"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobile_then_overrides() {
        let o = SessionOpts {
            mobile: true,
            scale: Some(2.0),
            ..Default::default()
        };
        let c = o.to_config(Path::new("/tmp")).unwrap();
        assert_eq!((c.width, c.height, c.scale), (390, 844, 2.0));
        assert_eq!(c.window_mode, WindowMode::Fullscreen);
    }

    #[test]
    fn env_and_points() {
        let e = parse_env(&["A=1".into(), "B=".into()]).unwrap();
        assert_eq!(e["A"].as_deref(), Some("1"));
        assert_eq!(e["B"], None);
        assert!(parse_env(&["nope".into()]).is_err());
        assert_eq!(parse_point("10, 20.5").unwrap(), (10.0, 20.5));
        assert!(parse_point("1,2,3").is_err());
        assert_eq!(parse_size("800x600").unwrap(), (800, 600));
    }
}
