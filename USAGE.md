# vway usage

- [Concepts](#concepts)
- [Output contract](#output-contract)
- [One-shot: `vway run`](#one-shot-vway-run)
- [Sessions](#sessions)
- [Command reference](#command-reference)
- [Scripts: `vway.action.v1`](#scripts-vwayactionv1)
- [MCP server](#mcp-server)
- [Claude's computer tool: `vway computer`](#claudes-computer-tool-vway-computer)
- [Coding agents: `vway agents`, `vway guide`](#coding-agents-vway-agents-vway-guide)
- [The app environment](#the-app-environment)
- [Rendering and GPUs](#rendering-and-gpus)
- [Environment variables](#environment-variables)
- [Error codes](#error-codes)
- [Troubleshooting](#troubleshooting)

## Concepts

- A **session** is one supervisor process. It runs a headless Wayland compositor with one output, a
  private D-Bus bus (by default), and at most one app. A session is addressed by its id.
- **Coordinates are logical pixels**, as on a desktop at scale 1. The low-level `screenshot`
  command captures **physical** pixels: a 1280×800 output at `--scale 1.5` gives 1920×1200 PNGs.
  Scales must be multiples of 1/120 (`1`, `1.25`, `1.5`, `2`, …), just like `wp_fractional_scale`.
- **Actions** (scripts, `run` flags, MCP, `vway computer`) work like Anthropic computer use:
  coordinates are pixels of the screenshots the actions return. Those screenshots have the
  output's logical size, shrunk to fit `--image-limit` if one is set, so at the defaults the two
  coordinate systems agree. See [Coordinates and image limits](#coordinates-and-image-limits).
- Windows are **maximized** to the output by default; `--window-mode fullscreen|floating` changes
  that. Dialogs (toplevels with a parent) float centered over their parent; popups are kept on
  screen.
- State lives under the **state root**: `$VWAY_STATE_DIR`, else `$XDG_RUNTIME_DIR/vway`, else
  `/tmp/vway-$UID`. Each session has a directory `sessions/<id>/` that holds:
  - `ctl.sock`: the control socket
  - `info.json`: written when the session is ready
  - `exit.json`: why the session ended
  - `compositor.log` and `app.log`
  - the private `home/` and `xdg/` runtime dir

## Output contract

Every command prints exactly **one JSON document to stdout**, and `"ok"` is always its first key.

- On failure, `ok` is false and the document carries
  `"error": {"code": "...", "message": "...", "details": {...}}`. A single line
  `vway: error[code]: message` also goes to stderr.
- Exit codes:
  - 0: success
  - 1: failure
  - 2: usage error (bad flags are reported as JSON too)

Diagnostics never go to stdout. The only commands that print text instead are `vway guide`,
`--help` and `--version`.

## One-shot: `vway run`

`vway run` starts a private session, launches the app and waits for its first window. Then it runs
the inline actions **in command-line order**, stops the session and prints everything:

```console
vway run --resolution 640x400 --screenshot a.png --click 100,100 --type hi -- ./my-app
```

```json
{
  "ok": true,
  "session": "run-fc2916",
  "launch": { "pid": 2059245, "warnings": [] },
  "window": { "id": 1, "title": "my-app", "app_id": "dev.example.app", "width": 640, "height": 400, "ready": true, "focused": true, ... },
  "results": [
    { "index": 0, "type": "screenshot", "ok": true, "result": { "width": 640, "height": 400, "scale": 1.0, "path": "/abs/a.png" } },
    { "index": 1, "type": "left_click", "ok": true, "result": { "commit_seq": 1 } },
    { "index": 2, "type": "type", "ok": true, "result": { "typed": 2, "commit_seq": 1 } }
  ],
  "app": { "pid": 2059245, "running": true, ... },
  "exit": { "reason": "shutdown_request", ... }
}
```

| Inline action (repeatable) | |
| --- | --- |
| `--screenshot FILE` | PNG, taken after the app settles |
| `--click X,Y` | left click |
| `--key KEYS` | key combo, e.g. `ctrl+s`, `Return` |
| `--type TEXT` | type text |
| `--wait-ms MS` | sleep |
| `--script FILE` | run a `vway.action.v1` script |

Other flags:

- `--until-exit`: wait for the app to exit on its own after the actions.
- `--timeout-ms`: how long to wait for the first window.
- `--out-dir`: where script screenshots without a path go (default `.`).
- `--image-limit standard|large|none`: shrink action screenshots to fit a model's image limit
  (default `none`); see [Coordinates and image limits](#coordinates-and-image-limits).
- `--keep`: keep the session directory.

`run` also takes every [session option](#session-options).

On success the session directory is removed. On failure it's kept, and the output includes `logs`
paths and an `app_log_tail`. The session belongs to the `vway run` process, so if `run` is killed,
the session dies with it.

## Sessions

Detached sessions outlive the command that started them. That's handy when you'd rather inspect
between steps:

```console
vway start --id demo --resolution 1280x800 -- ./my-app   # waits for the first window
vway screenshot -s demo -o shot.png
vway click -s demo 200,300
vway type -s demo "hello"
vway key -s demo ctrl+a
vway status -s demo
vway stop -s demo
```

`-s/--session` defaults to `$VWAY_SESSION`, or to the only running session. Apps launched in a
session get `VWAY_SESSION` set, so commands run from inside the app's process tree pick it up
automatically.

### Session options

These apply to `run` and `start`, and to the `start` tool in MCP:

| Option | Default | |
| --- | --- | --- |
| `--resolution WxH` | `1280x800` | logical output size |
| `--scale S` | `1` | output scale, multiples of 1/120 |
| `--mobile` | | 390×844 at scale 3, fullscreen windows |
| `--window-mode` | `maximized` | `maximized`, `fullscreen` or `floating` |
| `--gpu` | `auto` | `auto` or `software` (see [Rendering](#rendering-and-gpus)) |
| `--home` | `isolated` | `isolated` (a fresh per-session `HOME`), `host`, or a directory |
| `--dbus` | `private` | `private`, `host` or `none` |
| `--keymap` | `us` | xkb `LAYOUT[:VARIANT]`, e.g. `de:nodeadkeys` |
| `--exit-with-app` | off | end the session when the app exits |
| `--env K=V` | | session-wide app environment; `K=` unsets; repeatable |

`start` also takes `--id ID` (default random), `--replace` (stop a running session with that id
first) and `--timeout-ms`.

## Command reference

| Command | |
| --- | --- |
| `run [opts] -- CMD…` | one-shot (above) |
| `start [opts] [-- CMD…]` | start a detached session, optionally launching CMD |
| `launch [--env K=V] [--cwd DIR] [--no-wait] -- CMD…` | launch the session's app (one per session); the app inherits the caller's environment and cwd |
| `status` | output, pointer, focus, windows, app, `commit_seq`, `ms_since_commit`, `client_errors` |
| `wait window [--title T] [--app-id A]` | wait for a matching window with a buffer |
| `wait frame [--after-seq N]` | wait for the next commit |
| `wait idle [--quiet-ms MS]` | wait until no commits for a quiet period |
| `wait exit` | wait for the app to exit |
| `screenshot [-o FILE] [--region X,Y,W,H] [--no-settle]` | PNG; by default it waits up to 3 s for the app to go idle |
| `move X,Y` | pointer motion |
| `click [X,Y] [--button B] [--count N] [--mods ctrl+shift]` | buttons: `left`, `right`, `middle`, `back`, `forward` or an evdev code |
| `mouse down\|up [--button B]` | press or release a button |
| `drag X1,Y1 X2,Y2 [--duration-ms]` | left-button drag |
| `scroll up\|down\|left\|right [N] [--at X,Y]` | N wheel clicks (default 3) |
| `key KEYS [--repeat N] [--down\|--up]` | `ctrl+shift+e`, `Return`, `F5`, `super`; xkb keysym names (the names xdotool uses) work too |
| `type TEXT` | type text; `-` reads stdin. Characters the keymap can't produce fail with `unmappable_text` before anything is typed |
| `touch tap X,Y` | tap |
| `touch long-press X,Y [--duration-ms 800]` | press and hold |
| `touch swipe X1,Y1 X2,Y2 [--duration-ms 300]` | one-finger swipe |
| `touch pinch X,Y [--from 200] [--to 80] [--duration-ms 400]` | two-finger pinch; `to < from` zooms out |
| `resize WxH [--scale S]` | resize the output (prints the new output); windows get reconfigured |
| `focus WINDOW` / `close-window WINDOW` | by window id from `status` |
| `close-app [--grace-ms MS]` | SIGTERM, then SIGKILL after the grace period |
| `do SCRIPT [--out-dir DIR] [--image-limit L]` | run a script file (`-` for stdin) |
| `computer [FILE] [--image-limit L]` | answer Claude computer-use `tool_use` blocks (see [below](#claudes-computer-tool-vway-computer)) |
| `stop` / `stop --all` | stop sessions; prints the exit reason |
| `list [--all]` | sessions; `--all` includes stopped ones |
| `gc` | remove directories of stopped sessions |
| `doctor` | host checks: state root, socket paths, `vway` on `PATH` and on the desktop session's `PATH`, xkb, a D-Bus daemon, render node, Vulkan loader, lavapipe, Mesa EGL, agent skill. Failed checks carry a `fix` command for the detected distribution, and `install` combines them |
| `mcp [--allow PROG\|DIR/] [--image-limit L]` | MCP server on stdio |
| `agents setup\|status\|remove` | connect coding agents (see [below](#coding-agents-vway-agents-vway-guide)) |
| `guide [--skill]` | print the agent guide (Markdown) |

Modifier names are `shift`, `ctrl`/`control`, `alt`/`option`, `super`/`meta`/`cmd`/`win` and
`altgr`. Key aliases include `enter`, `esc`, `backspace`, `tab`, `space`, `del`, `up`/`down`/`left`/`right`,
`pgup`/`pgdn` and `home`/`end`; single characters (`a`, `/`) and xkb keysym names (`XF86AudioMute`)
work too.

Exit reasons, from `stop`, `exit.json` and `exit` in `run` output:

- `shutdown_request`
- `app_exited` (with `--exit-with-app`)
- `signal_term`, `signal_int`, `signal_hup`

## Scripts: `vway.action.v1`

A script is either a JSON array of actions or an object like this:

```json
{
  "vway_action": 1,
  "actions": [
    { "type": "vway_wait", "for": "window", "timeout_ms": 10000 },
    { "type": "left_click", "coordinate": [200, 300] },
    { "type": "type", "text": "hello" },
    { "type": "key", "text": "ctrl+s" },
    { "type": "screenshot", "path": "after-save.png" }
  ]
}
```

Actions run in order and **stop at the first failure**. The report gives one entry per action;
actions that never ran are marked `not_executed` with Anthropic's halt text ("Not executed: an
earlier computer action in this turn failed."). Unknown fields are rejected, so typos surface as
errors instead of being ignored.

| Action | Fields |
| --- | --- |
| `screenshot` | `region`, `path`, `settle` (default true) |
| `zoom` | `region` (required), `path`; returns the region at the output's full resolution |
| `left_click` `right_click` `middle_click` `double_click` `triple_click` | `coordinate`, `text` (modifiers held, e.g. `"shift"`) |
| `left_mouse_down` `left_mouse_up` | `coordinate` (optional; Anthropic's form has none and uses the pointer position) |
| `mouse_move` | `coordinate`, `duration_ms` (glide) |
| `left_click_drag` | `start_coordinate`, `coordinate`, `text` (modifiers), `duration_ms` |
| `cursor_position` | |
| `scroll` | `scroll_direction`, `scroll_amount`, `coordinate`, `text` (modifiers) |
| `type` | `text` |
| `key` | `text` (a combo, or several separated by spaces: `"ctrl+a BackSpace"`), `repeat` |
| `hold_key` | `text` (one or more keys), `duration` (seconds) |
| `wait` | `duration` (seconds) |
| `vway_wait` | `for` (`window`/`frame`/`idle`/`exit`), `timeout_ms`, `quiet_ms`, `title`, `app_id`, `after_seq` |
| `vway_touch` | `events`: one frame of raw touch points, `[{ "phase": "down"\|"move"\|"up"\|"cancel", "id": 0, "coordinate": [x, y] }]`; down on an active id or move/up on an unknown one is `bad_request` |
| `vway_gesture` | `kind` (`tap`, `long_press`, `swipe`, `pinch`), `coordinate`, `start_coordinate`, `duration_ms`, `from_distance`, `to_distance` |
| `vway_resize` | `width`, `height`, `scale` |
| `vway_key` | `keys`, `action` (`press`/`down`/`up`) |

The names, fields and units follow Anthropic's computer-use actions (all 17 of them), so an
agent's actions can be replayed as they are; `vway_*` actions and fields such as `duration_ms` and
`path` are extensions. `region` is `[x0, y0, x1, y1]`: the top-left and bottom-right corners. With
`vway do`, screenshots that have no `path` are saved to `<session>/shots/shot-NNN.png`.

### Coordinates and image limits

Every action coordinate (`coordinate`, `start_coordinate`, `region`, touch points and distances,
and `cursor_position`'s result) is a pixel of the screenshots that actions return, also after a
`zoom`. Those screenshots have the output's **logical** size: at `--scale 2` they are downsampled
from the physical frame, and `zoom` shows a region at full resolution.

Models only accept images up to a certain size; beyond it the API shrinks the image (and the
model's coordinates no longer match) or rejects it. `--image-limit` shrinks screenshots before they
leave vway and scales coordinates back, as Anthropic recommends:

| Limit | Largest screenshot | For |
| --- | --- | --- |
| `standard` | 1568 px long edge, 1.15 MP | every Claude model (default of `vway mcp`) |
| `large` | 2576 px long edge, 4784 visual tokens (≈3.75 MP) | Claude Opus 4.7 and later, every model that uses `computer_toolset_20260801` |
| `none` | the logical size | default of `do` and `run` |

A 1920×1080 session under `standard` gives 1429×804 screenshots, and a click at `[714, 402]`
lands at logical (960, 540). The default 1280×800 fits every limit unchanged.

## MCP server

```console
vway mcp                                  # any program may be launched
vway mcp --allow ./target/debug/ --allow foot
```

Example client configuration:

```json
{ "mcpServers": { "vway": { "command": "vway", "args": ["mcp", "--allow", "/path/to/project/target/"] } } }
```

| Tool | |
| --- | --- |
| `start` | start a session (session options as JSON fields: `resolution`, `scale`, `mobile`, …), optionally with `command` and `env`. Waits for the first window and returns a screenshot |
| `launch` | launch the app in an existing session |
| `computer` | one action in the `computer_20251124` input shape (`{"action": "left_click", "coordinate": [x, y]}`) or a batch (`{"actions": [...]}`) |
| `screenshot` | screenshot, optionally a `region` `[x0, y0, x1, y1]` |
| `status`, `stop`, `list` | |

- **Names.** Clients prefix tools with the server name (Claude Code: `mcp__vway__start`), so the
  tools have short names. Each also has a `title` ("Computer Use", "Start Desktop", …) and
  annotations (`readOnlyHint` for `screenshot`, `status` and `list`) for clients that show them.
- **Images.** Screenshots come back as MCP image content. In the JSON text, the result says which
  image it is (`"image": 1`). They follow `--image-limit` (default `standard`), and coordinates
  follow the screenshots.
- **Why one `computer` tool.** Over MCP the model sees vway's tool description, not Anthropic's
  built-in computer tool, so the names (prefixed by the client, e.g. `mcp__vway__computer`) cannot
  match the toolset anyway. One tool with Anthropic's action names and fields keeps tool lists short
  for every agent. For the built-in tool itself, use `vway computer`.
- **Owned sessions.** Sessions started by the server are named `mcp-…` and belong to it. They're
  stopped when stdin closes, and they also die if the server is killed.
- **`--allow`.** This resolves the program to a canonical path. It must then match an exact program
  or sit below a `DIR/` entry, and `LD_*`/`PATH` overrides are refused. It's a guard rail against
  accidents, not a sandbox.

## Claude's computer tool: `vway computer`

A program that calls the Claude API can declare Anthropic's own computer tool, which Claude is
trained on, and let vway execute the calls. Pass the response's `tool_use` blocks in (a block, an
array of blocks, or the whole message, on stdin or from a file); vway runs them in order and prints
the `tool_result` blocks to send back in the next user message:

```console
vway start --id app -- ./target/debug/my-app
echo "$RESPONSE_JSON" | vway computer -s app
```

```json
{ "ok": true, "tool_results": [
  { "type": "tool_result", "tool_use_id": "toolu_01…", "toolset_name": "computer",
    "content": [{ "type": "text", "text": "OK" }] },
  { "type": "tool_result", "tool_use_id": "toolu_02…", "toolset_name": "computer",
    "content": [{ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "…" } }] }
] }
```

- **Both request shapes.** `computer_toolset_20260801` (required by Claude Opus 5.5, Sonnet 5.5 and
  Haiku 5.5 on the Claude API) sends one block per action, named after the action, with
  `"toolset_name": "computer"`; results echo it. `computer_20251124` and `computer_20250124` send
  `name: "computer"` with `input.action`. Other blocks (text, thinking, other tools) are skipped.
- **Results.** `screenshot` and `zoom` return the image, `cursor_position` returns `[x, y]`, the
  rest `OK`. A failed action gets `is_error` and `Error: …`; the actions after it get the halt text.
  `ok` is false (exit status 1) if anything failed, but `tool_results` always answers every block.
- **Image limit.** Defaults to `large` when the blocks come from the toolset and `standard`
  otherwise. Keep it the same for a whole conversation, since coordinates refer to earlier
  screenshots.
- **Tool entry.** Declare `{"type": "computer_toolset_20260801"}` (no size: the toolset takes the
  screenshots as they are), or `computer_20251124` with `display_width_px`/`display_height_px` set
  to the screenshot size vway returns.

## Coding agents: `vway agents`, `vway guide`

Package managers don't write into your home directory, so connecting agents is an explicit step.
It is safe to repeat, and every command prints what it found and changed (`items`, each with a
`state` and an `action`).

```console
vway agents setup                  # install the skill for every agent found on this machine
vway agents setup --mcp            # also register `vway mcp` as an MCP server
vway agents setup --project --mcp  # into the current project instead of your home
vway agents setup --agent claude,codex --dry-run
vway agents status
vway agents remove
```

Agents are found by their CLI on `PATH` or their config directory. With `--agent` you choose them
yourself.

| Agent | Skill | MCP server (`--mcp`), user / `--project` |
| --- | --- | --- |
| Claude Code | `~/.claude/skills/vway` (`$CLAUDE_CONFIG_DIR/skills/vway`) | `claude mcp add -s user` / `.mcp.json` |
| Codex | `~/.agents/skills/vway` | `~/.codex/config.toml` (`$CODEX_HOME`) / `.codex/config.toml` |
| Gemini CLI | `~/.agents/skills/vway` | `~/.gemini/settings.json` / `.gemini/settings.json` |
| Cursor | `~/.agents/skills/vway` | `~/.cursor/mcp.json` / `.cursor/mcp.json` |
| OpenCode | `~/.agents/skills/vway` | `~/.config/opencode/opencode.json` / `opencode.json` |
| pi | `~/.agents/skills/vway` | `~/.pi/agent/mcp.json` (`$PI_CODING_AGENT_DIR`) / `.pi/mcp.json` |
| dsh | `~/.agents/skills/vway` | none: uses the skill |

With `--project`, skills go to `.agents/skills/vway` and `.claude/skills/vway` in the project.

- **The skill is a stub.** It says when to use vway and tells the agent to run `vway guide`, which
  prints the guide for the installed version. An upgrade never leaves an outdated skill behind.
- **Nothing that isn't vway's gets touched.** A `vway` skill that `setup` didn't write is
  reported and left alone; `--force` replaces it. An MCP entry named `vway` with a different
  command is handled the same way. JSON files with comments are never rewritten; the item tells
  you what to add by hand.
- **Claude Code's user config** (`~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json`) is only read; changes go through `claude mcp`.
- **The registered command** is `vway` when `PATH` runs this binary. For mise installs it is the
  mise shim, which keeps working across upgrades and without `mise activate`. Otherwise it is this
  binary's absolute path, with a warning. `--command` sets it explicitly, e.g.
  `--command .tools/bin/vway` for a project-local install.
- `vway agents remove` deletes only the skills `setup` wrote and the MCP entries named `vway`.
- `vway guide --skill` prints the stub, for agents that keep skills elsewhere.

Claude Code can also load vway as a plugin (`/plugin marketplace add chpr8/vway`, then
`/plugin install vway@vway`), with the same skill and the MCP server. Any other agent that reads
Agent Skills can use the repository's `skills/vway`.

## The app environment

The app gets the environment of whoever launched it. The CLI passes its own environment and working
directory along, and on top of that vway applies a fixed policy:

- `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` point at the session; `XDG_SESSION_TYPE=wayland`.
- Toolkit backend hints are set to Wayland: GTK, Qt, SDL, Clutter, Firefox and Electron.
- Host display variables are removed: `DISPLAY`, `WAYLAND_SOCKET`, `XAUTHORITY`, `SWAYSOCK`,
  `HYPRLAND_INSTANCE_SIGNATURE`, and the like.
- `--home isolated` sets `HOME` and the `XDG_*_HOME` variables to the session's `home/`, so the app
  sees no user config. Toolchains keep working: unless they are already set, `CARGO_HOME`,
  `RUSTUP_HOME` and `MISE_{DATA,CONFIG,STATE,CACHE}_DIR` point at the real directories when those
  exist, so `cargo run` inside a session doesn't download a fresh toolchain.
- `--dbus private` points `DBUS_SESSION_BUS_ADDRESS` at the session's own bus. This keeps
  single-instance apps from opening a window in an instance already running on your desktop.
- `MESA_SHADER_CACHE_DIR` is shared across sessions, which keeps GL and Vulkan startup fast.
- `--gpu software` adds Mesa's software drivers: llvmpipe, lavapipe, and the Mesa EGL vendor.
- `VWAY_SESSION` and `VWAY_STATE_DIR` are set.

Overrides are applied last, in this order: session `--env`, then `launch --env`. Each layer can set
or unset any variable, including the policy's.

## Rendering and GPUs

The compositor renders only when you ask for a screenshot, never continuously.

- **GLES** (default when `/dev/dri/renderD*` and EGL work): dmabufs are imported on the GPU,
  including tiled modifiers. It's also needed for Vulkan on drivers that allocate swapchain images
  where the CPU can't map them; AMD's RADV puts linear images in VRAM.
- **pixman** (`--gpu software`, no render node, or EGL failing): runs on the CPU and imports linear
  dmabufs by mapping them. If a client offers a buffer that can't be mapped, it's disconnected. The
  pending `start`/`wait` then fails right away with `protocol_error`, and `status` lists the error in
  `client_errors`.

`info.json` and `start` output report the renderer in use (`"renderer": "gles"`). Apps learn the
render node through dmabuf feedback, so GPU apps render on the same device.

`--gpu software` additionally steers the **app** onto Mesa's software rasterizers. That's
deterministic and runs anywhere, but it needs lavapipe for Vulkan apps such as GPUI; `vway doctor`
prints the package for your distribution.

## Environment variables

| Variable | |
| --- | --- |
| `VWAY_STATE_DIR` | state root (see [Concepts](#concepts)) |
| `VWAY_SESSION` | default session id for `-s` |
| `VWAY_LOG` | supervisor log filter, `tracing` syntax (default `info,smithay=warn`); logs go to `compositor.log` |
| `VWAY_RENDERER=pixman` | force the CPU renderer (debugging) |

## Error codes

| Code | Meaning |
| --- | --- |
| `usage` | bad command-line arguments (exit 2) |
| `bad_request`, `script_error` | malformed request or script |
| `session_not_found`, `session_exists` | no such session / id taken (use `--replace`) |
| `spawn_failed`, `connect_failed`, `path_too_long` | the supervisor didn't start or can't be reached; socket paths must fit 107 bytes |
| `no_app`, `app_running`, `launch_failed` | app lifecycle |
| `app_exited` | the app exited while you were waiting for it |
| `protocol_error` | a Wayland client was disconnected for a protocol error (also: malformed control replies) |
| `no_window` | no such window / nothing focused |
| `invalid_key`, `unmappable_text` | unknown key name / a character the keymap can't produce |
| `out_of_range` | coordinates, region, size or scale out of range |
| `timeout` | a wait ran out |
| `render_failed`, `io_error`, `shutting_down`, `internal` | |
| `not_executed` | a script action that never ran because an earlier one failed |
| `not_allowed` | MCP `--allow` refused the program |
| `unsupported_protocol` | CLI and supervisor versions differ |

## Troubleshooting

| Symptom | What to do |
| --- | --- |
| `wait window` times out | The app isn't a Wayland client, or it is still starting. Read `app.log` (failures include `app_log_tail`), and raise `--timeout-ms` for slow builds |
| `protocol_error` at startup | A GPU app's buffer couldn't be imported. Check `status` → `client_errors` and `compositor.log`; with pixman, try the default `--gpu auto` on a machine with a render node |
| "No GPU adapters found" (wgpu, GPUI) | No Vulkan driver for the device the app picked. With `--gpu software`, install lavapipe (see `vway doctor`) |
| The app opens on your real desktop | It reached the host bus or display. Keep `--dbus private` and don't pass `DISPLAY`/`WAYLAND_DISPLAY` through `--env` |
| `path_too_long` | Set `VWAY_STATE_DIR` to a shorter directory |
| `cargo run` in a session downloads a toolchain | The toolchain lives somewhere other than `~/.cargo`/`~/.rustup`; set `CARGO_HOME`/`RUSTUP_HOME` in your environment or pass them with `--env` |
| An agent doesn't know about vway | `vway agents status`, then `vway agents setup`; restart the agent so it reloads skills and MCP servers |
| A session survives a crash | `vway stop -s ID` or `vway stop --all`; `vway gc` removes leftover directories |
