//! Compositor state: globals, output, windows, window policy, and readiness counters.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::desktop::{PopupManager, Space, Window, WindowSurfaceType};
use smithay::input::keyboard::XkbConfig;
use smithay::input::{Seat, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::{LoopHandle, LoopSignal};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size, Transform};
use smithay::wayland::compositor::{
    CompositorClientState, CompositorState, SurfaceData, TraversalAction, send_surface_state,
    with_states, with_surface_tree_downward,
};
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufState};
use smithay::wayland::fractional_scale::{FractionalScaleManagerState, with_fractional_scale};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgShellState, XdgToplevelSurfaceData};
use smithay::wayland::shm::ShmState;
use smithay::wayland::viewporter::ViewporterState;
use tracing::{info, warn};
use vway_proto::{OutputInfo, WindowInfo};

use crate::config::{SessionConfig, WindowMode};
use crate::control::Control;
use crate::env::Policy;
use crate::keymap::ReverseKeymap;
use crate::process::{AppProc, Bus};
use crate::render::Renderer;

pub const REFRESH_MHZ: i32 = 60_000;
pub const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Per-window bookkeeping stored in the window's user data.
pub struct WindowMeta {
    pub id: u32,
    pub ready: Cell<bool>,
    pub mapped: Cell<bool>,
}

pub fn meta(w: &Window) -> &WindowMeta {
    w.user_data()
        .get::<WindowMeta>()
        .expect("every window has WindowMeta")
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
    pub errors: ClientErrors,
}

/// Protocol errors that got clients disconnected, shared between all clients and the state.
pub type ClientErrors = Arc<Mutex<Vec<(Instant, String)>>>;

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, reason: DisconnectReason) {
        if let DisconnectReason::ProtocolError(e) = reason {
            let msg = format!(
                "{}@{}: error {}: {}",
                e.object_interface, e.object_id, e.code, e.message
            );
            warn!("wayland client disconnected: {msg}");
            if let Ok(mut errors) = self.errors.lock() {
                errors.push((Instant::now(), msg));
                let excess = errors.len().saturating_sub(16);
                errors.drain(..excess);
            }
        }
    }
}

pub struct Paths {
    pub session: PathBuf,
    pub ctl_sock: PathBuf,
    pub app_log: PathBuf,
    /// Isolated home to delete at shutdown.
    pub temp_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Running,
    /// SIGTERM sent to the app; waiting for it (while still serving Wayland) until the deadline.
    Terminating(Instant),
}

pub struct State {
    pub cfg: SessionConfig,
    pub paths: Paths,
    pub started: Instant,
    pub dh: DisplayHandle,
    pub handle: LoopHandle<'static, State>,
    pub signal: LoopSignal,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    #[allow(dead_code)] // keeps the global alive
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub dmabuf_state: DmabufState,
    #[allow(dead_code)] // keeps the global alive
    pub dmabuf_global: DmabufGlobal,
    #[allow(dead_code)] // keeps the global alive
    pub viewporter_state: ViewporterState,
    #[allow(dead_code)] // keeps the global alive
    pub fractional_scale_state: FractionalScaleManagerState,

    pub seat: Seat<State>,
    pub output: Output,
    pub space: Space<Window>,
    pub popups: PopupManager,
    pub renderer: Renderer,
    pub keymap: ReverseKeymap,

    /// All toplevels in creation order (mapped or not).
    pub windows: Vec<Window>,
    next_window_id: u32,
    frame_count: u32,

    pub pointer: Point<f64, Logical>,
    pub commit_seq: u64,
    pub client_errors: ClientErrors,
    /// Touch ids currently down.
    pub touch_points: std::collections::BTreeSet<u32>,
    pub last_commit: Instant,
    pub last_input: Instant,

    pub app: Option<AppProc>,
    pub bus: Option<Bus>,
    pub policy: Policy,
    pub warnings: Vec<String>,

    pub control: Control,
    pub phase: Phase,
    pub shutdown_reason: Option<String>,
}

pub struct StateInit {
    pub cfg: SessionConfig,
    pub paths: Paths,
    pub dh: DisplayHandle,
    pub handle: LoopHandle<'static, State>,
    pub signal: LoopSignal,
    pub policy: Policy,
    pub bus: Option<Bus>,
    pub warnings: Vec<String>,
    pub control: Control,
}

impl State {
    pub fn new(init: StateInit) -> Result<State, String> {
        let StateInit {
            cfg,
            paths,
            dh,
            handle,
            signal,
            policy,
            bus,
            warnings,
            control,
        } = init;

        let compositor_state = CompositorState::new::<State>(&dh);
        let xdg_shell_state = XdgShellState::new::<State>(&dh);
        let shm_state = ShmState::new::<State>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<State>(&dh);
        let mut seat_state = SeatState::new();
        let data_device_state = DataDeviceState::new::<State>(&dh);
        let primary_selection_state = PrimarySelectionState::new::<State>(&dh);
        let viewporter_state = ViewporterState::new::<State>(&dh);
        let fractional_scale_state = FractionalScaleManagerState::new::<State>(&dh);

        let (renderer, node) = Renderer::new(cfg.gpu)?;
        let mut dmabuf_state = DmabufState::new();
        let formats = renderer.dmabuf_formats();
        // Feedback v4 names the GPU so EGL/Vulkan clients render on it (without a main device,
        // Mesa EGL cannot find its render node and falls back to software).
        let main_device = node.as_deref().and_then(|n| {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(n).ok().map(|m| m.rdev() as libc::dev_t)
        });
        let dmabuf_global = match main_device {
            Some(dev) => match DmabufFeedbackBuilder::new(dev, formats.clone()).build() {
                Ok(fb) => dmabuf_state.create_global_with_default_feedback::<State>(&dh, &fb),
                Err(e) => {
                    warn!("dmabuf feedback: {e}; falling back to linux-dmabuf v3");
                    dmabuf_state.create_global::<State>(&dh, formats)
                }
            },
            None => dmabuf_state.create_global::<State>(&dh, formats),
        };

        let keymap = ReverseKeymap::new(&cfg.keymap_layout, &cfg.keymap_variant)?;
        let mut seat: Seat<State> = seat_state.new_wl_seat(&dh, "seat0");
        seat.add_keyboard(
            XkbConfig {
                layout: &cfg.keymap_layout,
                variant: &cfg.keymap_variant,
                ..Default::default()
            },
            600,
            25,
        )
        .map_err(|e| format!("keyboard: {e:?}"))?;
        seat.add_pointer();
        seat.add_touch();

        let output = Output::new(
            "VWAY-1".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "vway".into(),
                model: "virtual".into(),
            },
        );
        let _global = output.create_global::<State>(&dh);
        let mut space = Space::default();

        let now = Instant::now();
        let mut state = State {
            cfg,
            paths,
            started: now,
            dh,
            handle,
            signal,
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            dmabuf_state,
            dmabuf_global,
            viewporter_state,
            fractional_scale_state,
            seat,
            output,
            space: Space::default(),
            popups: PopupManager::default(),
            renderer,
            keymap,
            windows: Vec::new(),
            next_window_id: 1,
            frame_count: 0,
            pointer: (0.0, 0.0).into(),
            commit_seq: 0,
            client_errors: ClientErrors::default(),
            touch_points: Default::default(),
            last_commit: now,
            last_input: now,
            app: None,
            bus,
            policy,
            warnings,
            control,
            phase: Phase::Running,
            shutdown_reason: None,
        };
        let (w, h, s) = (state.cfg.width, state.cfg.height, state.cfg.scale);
        state.apply_output(w, h, s);
        space.map_output(&state.output, (0, 0));
        state.space = space;
        let size = state.output_size();
        state.pointer = (size.w as f64 / 2.0, size.h as f64 / 2.0).into();
        Ok(state)
    }

    pub fn now_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    // --- output -------------------------------------------------------------------------------

    fn apply_output(&mut self, width: i32, height: i32, scale: f64) {
        let mode = Mode {
            size: physical_size(width, height, scale).into(),
            refresh: REFRESH_MHZ,
        };
        self.output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Fractional(scale)),
            Some((0, 0).into()),
        );
        self.output.set_preferred(mode);
        self.cfg.width = width;
        self.cfg.height = height;
        self.cfg.scale = scale;
    }

    pub fn output_size(&self) -> Size<i32, Logical> {
        (self.cfg.width, self.cfg.height).into()
    }

    pub fn output_info(&self) -> OutputInfo {
        let (pw, ph) = physical_size(self.cfg.width, self.cfg.height, self.cfg.scale);
        OutputInfo {
            width: self.cfg.width,
            height: self.cfg.height,
            scale: self.cfg.scale,
            physical_width: pw,
            physical_height: ph,
            refresh_mhz: REFRESH_MHZ,
        }
    }

    /// Change resolution and/or scale; reconfigure windows and re-announce surface scales.
    pub fn resize_output(&mut self, width: i32, height: i32, scale: f64) {
        self.apply_output(width, height, scale);
        self.space.map_output(&self.output, (0, 0));
        let out = self.output_size();
        for w in self.windows.clone() {
            let t = w.toplevel().expect("wayland window");
            let full = t.with_pending_state(|s| {
                s.states.contains(xdg_toplevel::State::Maximized)
                    || s.states.contains(xdg_toplevel::State::Fullscreen)
            });
            if full {
                t.with_pending_state(|s| s.size = Some(out));
            }
            if t.is_initial_configure_sent() {
                t.send_pending_configure();
            }
            if !full && meta(&w).mapped.get() {
                let loc = self.place(&w);
                self.space.map_element(w.clone(), loc, false);
            }
            self.update_surface_scale_tree(t.wl_surface());
        }
        let p = self.pointer;
        self.pointer = (
            p.x.min(out.w as f64 - 0.01).max(0.0),
            p.y.min(out.h as f64 - 0.01).max(0.0),
        )
            .into();
    }

    /// Tell a surface the output's scale (integer for `wl_surface`, exact for fractional-scale).
    pub fn update_surface_scale(&self, surface: &WlSurface) {
        with_states(surface, |states| self.apply_surface_scale(surface, states));
    }

    fn apply_surface_scale(&self, surface: &WlSurface, states: &SurfaceData) {
        let scale = self.cfg.scale;
        with_fractional_scale(states, |fs| fs.set_preferred_scale(scale));
        send_surface_state(surface, states, scale.ceil() as i32, Transform::Normal);
    }

    pub fn update_surface_scale_tree(&self, root: &WlSurface) {
        // The traversal holds each surface's state lock: use the `states` it passes, never
        // `with_states` (which would deadlock).
        with_surface_tree_downward(
            root,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, states, _| self.apply_surface_scale(surface, states),
            |_, _, _| true,
        );
        for (popup, _) in PopupManager::popups_for_surface(root) {
            self.update_surface_scale_tree(popup.wl_surface());
        }
    }

    // --- windows ------------------------------------------------------------------------------

    pub fn add_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        let id = self.next_window_id;
        self.next_window_id += 1;
        window.user_data().insert_if_missing(|| WindowMeta {
            id,
            ready: Cell::new(false),
            mapped: Cell::new(false),
        });
        info!(id, "new toplevel");
        self.windows.push(window);
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.windows
            .iter()
            .find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == surface))
            .cloned()
    }

    pub fn window_by_id(&self, id: u32) -> Option<Window> {
        self.windows.iter().find(|w| meta(w).id == id).cloned()
    }

    fn is_dialog(&self, w: &Window) -> bool {
        w.toplevel().and_then(|t| t.parent()).is_some()
    }

    /// Initial configure according to the window policy (design 0014 §2).
    pub fn initial_configure(&mut self, w: &Window) {
        let t = w.toplevel().expect("wayland window").clone();
        let dialog = self.is_dialog(w);
        let out = self.output_size();
        let mode = self.cfg.window_mode;
        let wl_output = self.output_resource_for(t.wl_surface());
        t.with_pending_state(|s| {
            s.states.set(xdg_toplevel::State::Activated);
            if dialog {
                return;
            }
            match mode {
                WindowMode::Maximized => {
                    s.size = Some(out);
                    s.states.set(xdg_toplevel::State::Maximized);
                }
                WindowMode::Fullscreen => {
                    s.size = Some(out);
                    s.states.set(xdg_toplevel::State::Fullscreen);
                    s.fullscreen_output = wl_output;
                }
                WindowMode::Floating => {}
            }
        });
        t.send_configure();
    }

    fn output_resource_for(
        &self,
        surface: &WlSurface,
    ) -> Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput> {
        let client = self
            .dh
            .get_client(smithay::reexports::wayland_server::Resource::id(surface))
            .ok()?;
        self.output.client_outputs(&client).next()
    }

    /// Where a window's geometry origin goes.
    fn place(&self, w: &Window) -> Point<i32, Logical> {
        let t = w.toplevel().expect("wayland window");
        let full = t.with_pending_state(|s| {
            s.states.contains(xdg_toplevel::State::Maximized)
                || s.states.contains(xdg_toplevel::State::Fullscreen)
        });
        if full {
            return (0, 0).into();
        }
        let size = w.geometry().size;
        let out = Rectangle::from_size(self.output_size());
        let parent_rect = t
            .parent()
            .and_then(|p| self.window_for_surface(&p))
            .and_then(|pw| self.space.element_geometry(&pw))
            .unwrap_or(out);
        let x = parent_rect.loc.x + (parent_rect.size.w - size.w) / 2;
        let y = parent_rect.loc.y + (parent_rect.size.h - size.h) / 2;
        let x = x.min(out.size.w - size.w).max(0);
        let y = y.min(out.size.h - size.h).max(0);
        (x, y).into()
    }

    /// Called after every commit to a toplevel's root surface.
    pub fn toplevel_committed(&mut self, w: &Window, attached_buffer: bool) {
        let m = meta(w);
        let t = w.toplevel().expect("wayland window");
        let has_buffer =
            with_renderer_surface_state(t.wl_surface(), |s| s.buffer().is_some()).unwrap_or(false);
        if (attached_buffer || has_buffer) && !m.ready.get() && has_buffer {
            info!(id = m.id, "window ready (first buffer)");
            m.ready.set(true);
        }
        if !m.mapped.get() && has_buffer {
            let loc = self.place(w);
            m.mapped.set(true);
            self.space.map_element(w.clone(), loc, false);
            self.focus_window(Some(w));
        } else if m.mapped.get() && !has_buffer {
            // A null buffer unmaps the window; it may map again later.
            m.mapped.set(false);
            m.ready.set(false);
            self.space.unmap_elem(w);
            self.refocus_after_unmap(w);
        }
        w.on_commit();
    }

    pub fn remove_toplevel(&mut self, surface: &ToplevelSurface) {
        let Some(w) = self.window_for_surface(surface.wl_surface()) else {
            return;
        };
        info!(id = meta(&w).id, "toplevel destroyed");
        self.windows.retain(|x| x != &w);
        self.space.unmap_elem(&w);
        self.refocus_after_unmap(&w);
    }

    fn refocus_after_unmap(&mut self, gone: &Window) {
        let focused = self.focused_window();
        if focused.is_none() || focused.as_ref() == Some(gone) {
            let top = self.space.elements().last().cloned();
            self.focus_window(top.as_ref());
        }
    }

    pub fn focused_window(&self) -> Option<Window> {
        let kb = self.seat.get_keyboard()?;
        let focus = kb.current_focus()?;
        self.window_for_surface(&focus).or_else(|| {
            // Focus may be on a popup; attribute it to its toplevel.
            let kind = self.popups.find_popup(&focus)?;
            let root = smithay::desktop::find_popup_root_surface(&kind).ok()?;
            self.window_for_surface(&root)
        })
    }

    /// Raise, activate and give keyboard focus to `w` (or clear focus).
    pub fn focus_window(&mut self, w: Option<&Window>) {
        if let Some(w) = w {
            self.space.raise_element(w, true);
        }
        for other in &self.windows {
            let t = other.toplevel().expect("wayland window");
            let active = Some(other) == w;
            t.with_pending_state(|s| {
                if active {
                    s.states.set(xdg_toplevel::State::Activated);
                } else {
                    s.states.unset(xdg_toplevel::State::Activated);
                }
            });
            if t.is_initial_configure_sent() {
                t.send_pending_configure();
            }
        }
        let surface = w.and_then(|w| w.toplevel().map(|t| t.wl_surface().clone()));
        if let Some(kb) = self.seat.get_keyboard() {
            if kb.is_grabbed() && surface.is_some() {
                // A popup grab owns the keyboard; it restores focus when it ends.
                return;
            }
            kb.set_focus(self, surface, SERIAL_COUNTER.next_serial());
        }
    }

    pub fn window_info(&self, w: &Window) -> WindowInfo {
        let m = meta(w);
        let t = w.toplevel().expect("wayland window");
        let (title, app_id) = with_states(t.wl_surface(), |states| {
            let data = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .expect("toplevel data")
                .lock()
                .expect("toplevel data lock");
            (data.title.clone(), data.app_id.clone())
        });
        let geo = self
            .space
            .element_geometry(w)
            .unwrap_or_else(|| Rectangle::from_size(w.geometry().size));
        WindowInfo {
            id: m.id,
            title,
            app_id,
            x: geo.loc.x,
            y: geo.loc.y,
            width: geo.size.w,
            height: geo.size.h,
            ready: m.ready.get(),
            focused: self.focused_window().as_ref() == Some(w),
            parent: t
                .parent()
                .and_then(|p| self.window_for_surface(&p))
                .map(|p| meta(&p).id),
        }
    }

    /// The surface under a logical point, with that surface's origin in global coordinates.
    pub fn surface_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (w, render_loc) = self.space.element_under(pos)?;
        w.surface_under(pos - render_loc.to_f64(), WindowSurfaceType::ALL)
            .map(|(s, loc)| (s, (loc + render_loc).to_f64()))
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<Window> {
        self.space.element_under(pos).map(|(w, _)| w.clone())
    }

    /// Frame callbacks for everything on screen.
    pub fn send_frames(&mut self) {
        let output = self.output.clone();
        let now = self.started.elapsed();
        for w in self.space.elements() {
            w.send_frame(&output, now, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
        self.space.refresh();
        self.popups.cleanup();
        self.frame_count = self.frame_count.wrapping_add(1);
        if self.frame_count.is_multiple_of(60) {
            self.renderer.cleanup();
        }
    }
}

pub fn physical_size(width: i32, height: i32, scale: f64) -> (i32, i32) {
    (
        (width as f64 * scale).round() as i32,
        (height as f64 * scale).round() as i32,
    )
}
