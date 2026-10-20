//! vway: headless Wayland desktops for testing and driving GUI apps.
//!
//! The binary (`main.rs`) is a thin CLI over these modules; the compositor itself lives in
//! `vway-compositor` and runs in the same executable as `vway __serve <dir>`.

#[cfg(not(target_os = "linux"))]
compile_error!(
    "vway runs on Linux only: it hosts a Wayland compositor and relies on Linux process APIs \
     (prctl, /proc). There is no macOS or Windows version."
);

#[cfg(target_os = "linux")]
pub mod actions;
#[cfg(target_os = "linux")]
pub mod agents;
#[cfg(target_os = "linux")]
pub mod client;
#[cfg(target_os = "linux")]
pub mod computer;
#[cfg(target_os = "linux")]
pub mod doctor;
#[cfg(target_os = "linux")]
pub mod mcp;
#[cfg(target_os = "linux")]
pub mod opts;
#[cfg(target_os = "linux")]
pub mod session;
