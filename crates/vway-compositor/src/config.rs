//! Session configuration, written by the spawner to `session.json` and read by `serve`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    #[default]
    Maximized,
    Fullscreen,
    Floating,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuMode {
    #[default]
    Auto,
    Software,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HomeMode {
    #[default]
    Isolated,
    Host,
    Dir(PathBuf),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbusMode {
    #[default]
    Private,
    Host,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    pub id: String,
    /// Logical output size.
    pub width: i32,
    pub height: i32,
    pub scale: f64,
    #[serde(default)]
    pub window_mode: WindowMode,
    #[serde(default)]
    pub gpu: GpuMode,
    #[serde(default)]
    pub home: HomeMode,
    #[serde(default)]
    pub dbus: DbusMode,
    /// xkb layout and optional variant.
    #[serde(default = "default_layout")]
    pub keymap_layout: String,
    #[serde(default)]
    pub keymap_variant: String,
    #[serde(default)]
    pub exit_with_app: bool,
    /// State root, for the shared Mesa shader cache.
    pub state_root: PathBuf,
    /// Session-wide `--env` overrides, applied after the policy and before per-launch overrides.
    #[serde(default)]
    pub env: BTreeMap<String, Option<String>>,
}

fn default_layout() -> String {
    "us".into()
}

impl SessionConfig {
    pub fn new(id: impl Into<String>, state_root: PathBuf) -> Self {
        SessionConfig {
            id: id.into(),
            width: 1280,
            height: 800,
            scale: 1.0,
            window_mode: WindowMode::default(),
            gpu: GpuMode::default(),
            home: HomeMode::default(),
            dbus: DbusMode::default(),
            keymap_layout: default_layout(),
            keymap_variant: String::new(),
            exit_with_app: false,
            state_root,
            env: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(1..=16384).contains(&self.width) || !(1..=16384).contains(&self.height) {
            return Err(format!(
                "resolution {}x{} out of range",
                self.width, self.height
            ));
        }
        validate_scale(self.scale)?;
        let pw = (self.width as f64 * self.scale).round();
        let ph = (self.height as f64 * self.scale).round();
        if pw > 16384.0 || ph > 16384.0 {
            return Err(format!("physical size {pw}x{ph} exceeds 16384"));
        }
        Ok(())
    }
}

pub fn validate_scale(scale: f64) -> Result<(), String> {
    if !(0.25..=8.0).contains(&scale) || !scale.is_finite() {
        return Err(format!("scale {scale} out of range (0.25..=8)"));
    }
    // wp_fractional_scale_v1 works in 1/120 steps.
    if ((scale * 120.0).round() - scale * 120.0).abs() > 1e-6 {
        return Err(format!("scale {scale} is not a multiple of 1/120"));
    }
    Ok(())
}
