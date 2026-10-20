# vway

A headless Wayland compositor for testing and driving GUI apps.

---

> [!WARNING]
> vway is alpha software. Expect bugs and breaking changes to the CLI, JSON output and MCP tools.

vway runs a GUI app on its own private, headless Wayland desktop and lets you (or an agent) look at it
and use it. You can take screenshots and send pointer, keyboard, touch and gesture input, resize the
display, and wait for windows, frames or idleness. Nothing appears on your screen, and nothing touches
your input devices.

You can drive it through a CLI that prints one JSON document per command, through
`vway.action.v1` scripts, or through an MCP server (`vway mcp`) with an Anthropic-compatible
`computer` tool.

```console
$ vway run --resolution 1280x800 --screenshot shot.png --click 640,400 --type "hello" -- my-app
{ "ok": true, "session": "run-3f9a1c", "window": { "title": "my-app", "ready": true, ... }, "results": [...] }
```

## Features

- **Private sessions.** Each session gets its own compositor, `XDG_RUNTIME_DIR`, `HOME` and D-Bus
  session bus. Display-server variables from the host (`DISPLAY`, `SWAYSOCK`, …) are removed, so apps
  can't reach your real desktop.
- **Real input.** Input arrives through `wl_pointer`, `wl_keyboard` and `wl_touch`. Text is typed as
  key events for the session's xkb layout, so apps see what a person typing would produce.
- **GPU or CPU.** On a machine with a GPU, the compositor renders with GLES, so OpenGL and Vulkan apps
  can hand it dmabufs, including tiled ones. Without a GPU it falls back to pixman on the CPU, and
  `--gpu software` puts the app on Mesa's software drivers as well.
- **Fractional scaling and mobile.** `--scale 1.5`, `--mobile` (390×844 at scale 3 with touch), and
  resizing at runtime.
- **Honest synchronization.** You can wait for a ready window, the next frame or an idle app, and
  screenshots wait for the app to settle first. When an app exits or a client hits a protocol error,
  the wait fails right away with an error code instead of a timeout.
- **Agent-ready.** `vway mcp` gives an agent a `computer` tool that speaks Anthropic computer-use
  actions (all 17, same fields, units and coordinate rules) plus `vway_*` extensions. Sessions die
  with the server, and `--allow` limits which programs it can launch. Programs on the Claude API can
  declare Anthropic's built-in computer tool and pipe its calls through `vway computer`.

Linux only (any distribution, including WSL2). Apps must be Wayland clients; X11 and XWayland aren't
supported. The host itself needs no display server: vway works the same on an X11 desktop, over SSH
and in CI.

## Install

Pick one; each puts a single `vway` binary on your `PATH`:

```console
cargo install --locked vway                 # from source
mise use -g github:chpr8/vway               # prebuilt (x86_64, aarch64); or cargo:vway
```

Building from source needs `pkg-config` and the libxkbcommon and pixman development packages
(`libxkbcommon-dev libpixman-1-dev` on Debian/Ubuntu). Then check the host:

```console
vway doctor        # what is missing (Vulkan loader, lavapipe, D-Bus, …), with the install command
```

vway never installs system packages. Optional pieces only matter for what runs inside a session:
`dbus-daemon` for private session buses, a GPU render node with Mesa EGL for GPU compositing, the
Vulkan loader for Vulkan apps such as GPUI, and Mesa's lavapipe for `--gpu software`.

### Per project

To pin vway for a project instead, use mise (`mise use github:chpr8/vway@0.1` writes `mise.toml`) or
install into the project: `cargo install --locked --root .tools vway` gives `.tools/bin/vway`.

### Connect your coding agents

Installers can't write to your home directory, so connecting agents is one explicit step:

```console
vway agents setup          # the skill, for every agent found (Claude Code, Codex, Cursor, Gemini CLI, OpenCode, pi, dsh)
vway agents setup --mcp    # … and register `vway mcp` as an MCP server
vway agents setup --project --mcp   # into this project (.agents/skills, .claude/skills, .mcp.json, …)
vway agents status         # what is installed where;  vway agents remove  undoes setup
```

The installed skill is short: it tells the agent to run `vway guide`, which prints the reference
for the installed version, so upgrading vway never leaves a stale skill behind. Claude Code users
can install the plugin instead (`/plugin marketplace add chpr8/vway`); it bundles the skill and
the MCP server, and still needs the binary from above.

## Documentation

- [`USAGE.md`](USAGE.md): concepts, every command, scripts, MCP, environment and troubleshooting.
- `vway guide`: the guide for coding agents ([`skills/vway/SKILL.md`](skills/vway/SKILL.md) is the
  skill that points to it).
- [`packaging/`](packaging/README.md): release channels and the release checklist.

## Layout

```
crates/vway-proto        control protocol types (JSON over a Unix socket)
crates/vway-compositor   the session supervisor: smithay compositor, input, rendering, app lifecycle
crates/vway              the vway binary: CLI, action runner, MCP server, agent setup; tests and
                         the test client; agent/ holds the embedded skill and guide
skills/vway              the agent skill (a copy of crates/vway/agent/SKILL.md)
.claude-plugin           Claude Code plugin and marketplace manifests
packaging                packaging notes
.github/workflows        CI and release builds
```

## Development

```console
make test          # unit + end-to-end tests (spawn real sessions with the bundled test client)
make real-apps     # opt-in tests against foot, alacritty and vkcube, if they are installed
make clippy fmt
```

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. See
[`NOTICE`](NOTICE).
