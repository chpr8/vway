//! Minimal Wayland client for vway's end-to-end tests.
//!
//! Draws solid-color shm windows (with a white 10x10 logical marker at the top-left corner so
//! tests can catch flips and scaling errors) and logs every input event as a JSON line prefixed
//! with `VWAY_TEST ` to stdout and, if set, to the file `$VWAY_TEST_LOG`.
//!
//! Options:
//!   --color RRGGBB              fill color (default ff0000)
//!   --click-color RRGGBB        switch to this color on a button press or touch down
//!   --first-frame-delay-ms N    wait before attaching the first buffer
//!   --windows N                 number of toplevels (default 1)
//!   --title T, --app-id A       toplevel title/app id (window i > 0 gets a "-i" suffix)
//!   --duration MS               exit with status 0 after MS
//!   --exit-code N               exit status used by --duration (default 0)
//!   --env-log                   log selected environment variables at startup

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::{self, WlBuffer},
    wl_compositor::WlCompositor,
    wl_keyboard::{self, WlKeyboard},
    wl_pointer::{self, WlPointer},
    wl_registry::WlRegistry,
    wl_seat::{self, WlSeat},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
    wl_touch::{self, WlTouch},
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};
use xkbcommon::xkb;

struct Opts {
    color: u32,
    click_color: Option<u32>,
    first_frame_delay: Duration,
    windows: usize,
    title: String,
    app_id: String,
    duration: Option<Duration>,
    exit_code: i32,
    env_log: bool,
}

fn parse_color(s: &str) -> u32 {
    u32::from_str_radix(s.trim_start_matches('#'), 16).expect("color must be RRGGBB")
}

fn parse_opts() -> Opts {
    let mut o = Opts {
        color: 0xff0000,
        click_color: None,
        first_frame_delay: Duration::ZERO,
        windows: 1,
        title: "vway-testclient".into(),
        app_id: "dev.vway.testclient".into(),
        duration: None,
        exit_code: 0,
        env_log: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--color" => o.color = parse_color(&val()),
            "--click-color" => o.click_color = Some(parse_color(&val())),
            "--first-frame-delay-ms" => {
                o.first_frame_delay = Duration::from_millis(val().parse().expect("ms"))
            }
            "--windows" => o.windows = val().parse().expect("count"),
            "--title" => o.title = val(),
            "--app-id" => o.app_id = val(),
            "--duration" => o.duration = Some(Duration::from_millis(val().parse().expect("ms"))),
            "--exit-code" => o.exit_code = val().parse().expect("code"),
            "--env-log" => o.env_log = true,
            other => panic!("unknown option {other}"),
        }
    }
    o
}

struct Log(Option<File>);

impl Log {
    fn event(&mut self, name: &str, mut fields: Value) {
        fields["event"] = json!(name);
        let line = format!("VWAY_TEST {fields}\n");
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
        if let Some(f) = &mut self.0 {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
        }
    }
}

struct Window {
    idx: usize,
    surface: WlSurface,
    toplevel: XdgToplevel,
    xdg: XdgSurface,
    viewport: Option<WpViewport>,
    _fractional: Option<WpFractionalScaleV1>,
    width: i32,
    height: i32,
    scale120: u32,
    configured: bool,
    color: u32,
}

struct App {
    opts: Opts,
    log: Log,
    compositor: WlCompositor,
    shm: WlShm,
    wm: XdgWmBase,
    viewporter: Option<WpViewporter>,
    fractional: Option<WpFractionalScaleManagerV1>,
    windows: Vec<Window>,
    first_frame_at: Instant,
    xkb_context: xkb::Context,
    xkb_state: Option<xkb::State>,
    pointer_window: Option<usize>,
    pointer_pos: (f64, f64),
    touch_window: Option<usize>,
    exit: bool,
}

impl App {
    fn window_for(&self, surface: &WlSurface) -> Option<usize> {
        self.windows.iter().position(|w| &w.surface == surface)
    }

    fn may_draw(&self) -> bool {
        Instant::now() >= self.first_frame_at
    }

    fn draw(&mut self, idx: usize, qh: &QueueHandle<App>) {
        if !self.may_draw() {
            return;
        }
        let w = &self.windows[idx];
        if !w.configured {
            return;
        }
        let (lw, lh) = (w.width.max(1), w.height.max(1));
        let scale = w.scale120 as f64 / 120.0;
        let (bw, bh) = (
            (lw as f64 * scale).round() as i32,
            (lh as f64 * scale).round() as i32,
        );
        let stride = bw * 4;
        let size = (stride * bh) as usize;

        let fd = unsafe { libc::memfd_create(c"vway-testclient".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create failed");
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let file = File::from(fd);
        file.set_len(size as u64).expect("ftruncate");
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert!(ptr != libc::MAP_FAILED, "mmap failed");
        let pixels = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u32, size / 4) };
        let marker = (10.0 * scale).round() as i32;
        for y in 0..bh {
            for x in 0..bw {
                pixels[(y * bw + x) as usize] = if x < marker && y < marker {
                    0xffffffff
                } else {
                    0xff000000 | w.color
                };
            }
        }
        unsafe { libc::munmap(ptr, size) };

        let pool = self.shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(0, bw, bh, stride, wl_shm::Format::Xrgb8888, qh, ());
        pool.destroy();

        match &w.viewport {
            Some(vp) => {
                w.surface.set_buffer_scale(1);
                vp.set_destination(lw, lh);
            }
            None => w.surface.set_buffer_scale(1),
        }
        w.surface.attach(Some(&buffer), 0, 0);
        w.surface.damage_buffer(0, 0, bw, bh);
        w.surface.commit();
        let (idx, color) = (w.idx, w.color);
        self.log.event(
            "draw",
            json!({ "window": idx, "width": lw, "height": lh, "buffer_width": bw,
                    "buffer_height": bh, "color": format!("{color:06x}") }),
        );
    }

    fn draw_all(&mut self, qh: &QueueHandle<App>) {
        for i in 0..self.windows.len() {
            self.draw(i, qh);
        }
    }

    fn press(&mut self, idx: Option<usize>, qh: &QueueHandle<App>) {
        if let (Some(i), Some(c)) = (idx, self.opts.click_color)
            && self.windows[i].color != c
        {
            self.windows[i].color = c;
            self.draw(i, qh);
        }
    }

    fn mods(&self) -> Vec<&'static str> {
        let Some(st) = &self.xkb_state else {
            return vec![];
        };
        [
            (xkb::MOD_NAME_CTRL, "ctrl"),
            (xkb::MOD_NAME_SHIFT, "shift"),
            (xkb::MOD_NAME_ALT, "alt"),
            (xkb::MOD_NAME_LOGO, "super"),
        ]
        .into_iter()
        .filter(|(m, _)| st.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE))
        .map(|(_, n)| n)
        .collect()
    }
}

fn main() {
    let opts = parse_opts();
    let log_file = std::env::var_os("VWAY_TEST_LOG").map(|p| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .expect("open VWAY_TEST_LOG")
    });
    let mut log = Log(log_file);
    if opts.env_log {
        let vars = [
            "HOME",
            "XDG_RUNTIME_DIR",
            "WAYLAND_DISPLAY",
            "DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
            "XDG_SESSION_TYPE",
            "VK_DRIVER_FILES",
            "LIBGL_ALWAYS_SOFTWARE",
            "MESA_SHADER_CACHE_DIR",
            "VWAY_SESSION",
            "VWAY_TEST_MARK",
        ];
        let env: serde_json::Map<String, Value> = vars
            .iter()
            .map(|k| {
                (
                    k.to_string(),
                    std::env::var(k).map(Value::from).unwrap_or(Value::Null),
                )
            })
            .collect();
        let cwd = std::env::current_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        log.event("env", json!({ "vars": env, "cwd": cwd }));
    }

    let conn = Connection::connect_to_env().expect("connect to the Wayland display");
    let (globals, mut queue) = registry_queue_init::<App>(&conn).expect("registry");
    let qh = queue.handle();
    let compositor: WlCompositor = globals.bind(&qh, 4..=6, ()).expect("wl_compositor");
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).expect("wl_shm");
    let wm: XdgWmBase = globals.bind(&qh, 1..=6, ()).expect("xdg_wm_base");
    let seat: WlSeat = globals.bind(&qh, 1..=8, ()).expect("wl_seat");
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    let fractional: Option<WpFractionalScaleManagerV1> = globals.bind(&qh, 1..=1, ()).ok();
    let _ = seat;

    let first_frame_at = Instant::now() + opts.first_frame_delay;
    let deadline = opts.duration.map(|d| Instant::now() + d);
    let mut app = App {
        log,
        compositor,
        shm,
        wm,
        viewporter,
        fractional,
        windows: Vec::new(),
        first_frame_at,
        xkb_context: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb_state: None,
        pointer_window: None,
        pointer_pos: (0.0, 0.0),
        touch_window: None,
        exit: false,
        opts,
    };
    for idx in 0..app.opts.windows {
        let surface = app.compositor.create_surface(&qh, ());
        let fractional = app
            .fractional
            .as_ref()
            .map(|m| m.get_fractional_scale(&surface, &qh, idx));
        let viewport = app
            .viewporter
            .as_ref()
            .map(|v| v.get_viewport(&surface, &qh, ()));
        let xdg = app.wm.get_xdg_surface(&surface, &qh, idx);
        let toplevel = xdg.get_toplevel(&qh, idx);
        let suffix = if idx == 0 {
            String::new()
        } else {
            format!("-{idx}")
        };
        toplevel.set_title(format!("{}{suffix}", app.opts.title));
        toplevel.set_app_id(format!("{}{suffix}", app.opts.app_id));
        surface.commit();
        app.windows.push(Window {
            idx,
            surface,
            toplevel,
            xdg,
            viewport,
            _fractional: fractional,
            width: 0,
            height: 0,
            scale120: 120,
            configured: false,
            color: app.opts.color,
        });
    }
    app.log.event(
        "started",
        json!({ "windows": app.opts.windows, "pid": std::process::id() }),
    );

    let mut first_drawn = app.opts.first_frame_delay.is_zero();
    while !app.exit {
        queue.flush().expect("flush");
        queue.dispatch_pending(&mut app).expect("dispatch");
        if app.exit {
            break;
        }
        let now = Instant::now();
        if let Some(d) = deadline
            && now >= d
        {
            app.log.event("exit", json!({ "reason": "duration" }));
            queue.flush().ok();
            std::process::exit(app.opts.exit_code);
        }
        if !first_drawn && app.may_draw() {
            first_drawn = true;
            app.draw_all(&qh);
            continue;
        }
        let mut timeout: Option<Duration> = deadline.map(|d| d.saturating_duration_since(now));
        if !first_drawn {
            let t = app.first_frame_at.saturating_duration_since(now);
            timeout = Some(timeout.map_or(t, |x| x.min(t)));
        }
        // Requests made while dispatching must reach the compositor before we block.
        queue.flush().expect("flush");
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut pfd = libc::pollfd {
            fd: guard.connection_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.map_or(-1, |t| t.as_millis().min(i32::MAX as u128) as i32);
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n > 0 {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    app.log
                        .event("disconnected", json!({ "error": e.to_string() }));
                    std::process::exit(1);
                }
            }
        }
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for App {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(App: ignore WlCompositor);
delegate_noop!(App: ignore WlShm);
delegate_noop!(App: ignore WlShmPool);
delegate_noop!(App: ignore WlSurface);
delegate_noop!(App: ignore WpViewporter);
delegate_noop!(App: ignore WpViewport);
delegate_noop!(App: ignore WpFractionalScaleManagerV1);

impl Dispatch<WlBuffer, ()> for App {
    fn event(
        _: &mut Self,
        buffer: &WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
        }
    }
}

impl Dispatch<XdgWmBase, ()> for App {
    fn event(
        _: &mut Self,
        wm: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, usize> for App {
    fn event(
        app: &mut Self,
        xdg: &XdgSurface,
        event: xdg_surface::Event,
        idx: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            let w = &mut app.windows[*idx];
            if w.width == 0 || w.height == 0 {
                w.width = 400;
                w.height = 300;
            }
            w.configured = true;
            let (width, height, scale) = (w.width, w.height, w.scale120);
            app.log.event(
                "configure",
                json!({ "window": idx, "width": width, "height": height, "scale120": scale }),
            );
            app.draw(*idx, qh);
        }
    }
}

impl Dispatch<XdgToplevel, usize> for App {
    fn event(
        app: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                let w = &mut app.windows[*idx];
                if width > 0 && height > 0 {
                    w.width = width;
                    w.height = height;
                }
                let activated = states
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                    .any(|s| s == xdg_toplevel::State::Activated as u32);
                app.log.event("toplevel_configure", json!({ "window": idx, "width": width, "height": height, "activated": activated }));
            }
            xdg_toplevel::Event::Close => {
                app.log.event("close", json!({ "window": idx }));
                let w = &app.windows[*idx];
                w.toplevel.destroy();
                w.xdg.destroy();
                w.surface.destroy();
                app.windows[*idx].configured = false;
                if app.windows.iter().all(|w| !w.configured) {
                    app.exit = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, usize> for App {
    fn event(
        app: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        idx: &usize,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            app.log.event(
                "preferred_scale",
                json!({ "window": idx, "scale120": scale }),
            );
            if app.windows[*idx].scale120 != scale {
                app.windows[*idx].scale120 = scale;
                app.draw(*idx, qh);
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for App {
    fn event(
        _: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
            if caps.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(qh, ());
            }
            if caps.contains(wl_seat::Capability::Touch) {
                seat.get_touch(qh, ());
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for App {
    fn event(
        app: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                // SAFETY: the compositor hands us a keymap fd of `size` bytes.
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &app.xkb_context,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                };
                if let Ok(Some(keymap)) = keymap {
                    app.xkb_state = Some(xkb::State::new(&keymap));
                    app.log.event("keymap", json!({}));
                }
            }
            wl_keyboard::Event::Enter { surface, .. } => {
                let idx = app.window_for(&surface);
                app.log
                    .event("focus", json!({ "window": idx, "focused": true }));
            }
            wl_keyboard::Event::Leave { surface, .. } => {
                let idx = app.window_for(&surface);
                app.log
                    .event("focus", json!({ "window": idx, "focused": false }));
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(st) = &mut app.xkb_state {
                    st.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Key { key, state, .. } => {
                let pressed = matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                let code = xkb::Keycode::new(key + 8);
                let (sym, utf8) = match &app.xkb_state {
                    Some(st) => (
                        xkb::keysym_get_name(st.key_get_one_sym(code)),
                        st.key_get_utf8(code),
                    ),
                    None => (String::new(), String::new()),
                };
                let mods = app.mods();
                app.log.event(
                    "key",
                    json!({ "key": key, "sym": sym, "utf8": utf8, "pressed": pressed, "mods": mods }),
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for App {
    fn event(
        app: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                app.pointer_window = app.window_for(&surface);
                app.pointer_pos = (surface_x, surface_y);
                app.log.event(
                    "pointer_enter",
                    json!({ "window": app.pointer_window, "x": surface_x, "y": surface_y }),
                );
            }
            wl_pointer::Event::Leave { .. } => {
                app.log
                    .event("pointer_leave", json!({ "window": app.pointer_window }));
                app.pointer_window = None;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                app.pointer_pos = (surface_x, surface_y);
                app.log.event(
                    "motion",
                    json!({ "window": app.pointer_window, "x": surface_x, "y": surface_y }),
                );
            }
            wl_pointer::Event::Button { button, state, .. } => {
                let pressed = matches!(state, WEnum::Value(wl_pointer::ButtonState::Pressed));
                let (x, y) = app.pointer_pos;
                app.log.event("button", json!({ "window": app.pointer_window, "button": button, "pressed": pressed, "x": x, "y": y }));
                if pressed {
                    app.press(app.pointer_window, qh);
                }
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let axis = match axis {
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => "horizontal",
                    _ => "vertical",
                };
                app.log.event(
                    "axis",
                    json!({ "window": app.pointer_window, "axis": axis, "value": value }),
                );
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                let axis = match axis {
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => "horizontal",
                    _ => "vertical",
                };
                app.log.event(
                    "axis120",
                    json!({ "window": app.pointer_window, "axis": axis, "value120": value120 }),
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<WlTouch, ()> for App {
    fn event(
        app: &mut Self,
        _: &WlTouch,
        event: wl_touch::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_touch::Event::Down {
                surface, id, x, y, ..
            } => {
                app.touch_window = app.window_for(&surface);
                app.log.event(
                    "touch_down",
                    json!({ "window": app.touch_window, "id": id, "x": x, "y": y }),
                );
                app.press(app.touch_window, qh);
            }
            wl_touch::Event::Motion { id, x, y, .. } => {
                app.log
                    .event("touch_motion", json!({ "id": id, "x": x, "y": y }));
            }
            wl_touch::Event::Up { id, .. } => {
                app.log.event("touch_up", json!({ "id": id }));
            }
            wl_touch::Event::Frame => app.log.event("touch_frame", json!({})),
            wl_touch::Event::Cancel => app.log.event("touch_cancel", json!({})),
            _ => {}
        }
    }
}
