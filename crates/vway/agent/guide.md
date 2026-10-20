# vway guide

`vway` starts a private Wayland desktop, launches one app in it, and gives you its pixels and a real
seat for keyboard, pointer and touch. Every command prints **one JSON document on stdout**, with
`"ok"` first. On failure it also prints `vway: error[code]: message` on stderr and exits non-zero.

## Pick a mode

**One shot.** One command starts the desktop, launches the app, waits for the first window, runs the
actions in order, and cleans up:

```console
vway run --resolution 1280x800 --screenshot before.png --click 640,400 --type "hello" \
  --key ctrl+s --screenshot after.png -- ./target/debug/my-app
```

**A detached session** suits looking, thinking and acting over several steps:

```console
vway start --id app --resolution 1280x800 -- cargo run --example hello   # waits for the window
vway screenshot -s app -o shot.png          # then read the image
vway click -s app 200,300
vway type -s app "hello"
vway key -s app Return
vway status -s app                          # windows, focus, app state, client_errors
vway stop -s app
```

If your sandbox kills background processes when a command returns, stay with `vway run`. You can
also put the steps in a script and run it with `vway run --script steps.json -- APP`.

**Script.** Use `vway do steps.json -s app`, or `--script` with `run`. The format is `vway.action.v1`:
Anthropic computer-use actions with their names, fields and units, plus `vway_*` extensions:

```json
{ "vway_action": 1, "actions": [
  { "type": "left_click", "coordinate": [200, 300] },
  { "type": "type", "text": "hello" },
  { "type": "key", "text": "ctrl+a" },
  { "type": "scroll", "coordinate": [640, 400], "scroll_direction": "down", "scroll_amount": 5 },
  { "type": "vway_gesture", "kind": "pinch", "coordinate": [195, 420] },
  { "type": "zoom", "region": [0, 0, 400, 200] },
  { "type": "screenshot", "path": "result.png" }
] }
```

The script stops at the first failure; later actions are reported as `not_executed`. `region` is
`[x0, y0, x1, y1]` (two corners); `zoom` shows it at full resolution. `key` takes a combo or several
separated by spaces (`"ctrl+a BackSpace"`).

**MCP.** `vway mcp` exposes `start` (pass `command`; it returns a screenshot), `computer`
(one action as `{"action": "left_click", "coordinate": [x, y]}`, or `actions: [...]`),
`screenshot`, `status` and `stop`; clients prefix them, e.g. `mcp__vway__start`.

**Claude API programs** that declare Anthropic's computer tool (`computer_toolset_20260801` or
`computer_20251124`) can pipe the response's `tool_use` blocks into `vway computer -s ID` and send
back the `tool_result` blocks it prints.

## Useful flags

- `--scale 1.5` / `--scale 2` for HiDPI (scales are multiples of 1/120). `--mobile` gives a 390×844
  phone at scale 3 with touch.
- `--gpu software` makes rendering deterministic (llvmpipe/lavapipe), at some cost in speed. Vulkan
  apps such as GPUI need lavapipe; `vway doctor` says whether it is installed.
- `--env K=V` sets app env (`K=` unsets). `launch`/`start` pass your environment and cwd to the app.
- `--keymap de` to type through a different layout.
- `wait window|frame|idle|exit` to synchronize; screenshots already wait up to 3 s for idle.
- `touch tap|long-press|swipe|pinch`, `drag`, `scroll down 5 --at X,Y`, `click --count 2`,
  `click --mods ctrl`.
- `resize 800x600 --scale 2` to test layout changes at runtime.

## Rules

- **Actions** (scripts, `run` flags, MCP, `vway computer`): coordinates are pixels of the
  screenshots the actions return. Read them off the image and use them as they are.
- **Low-level commands** (`click`, `move`, `drag`, `scroll`, `touch`) take logical pixels, and
  `vway screenshot` is physical: at scale 2 that PNG is twice the size, so halve what you read off
  it. At scale 1 everything agrees.
- Large sessions (e.g. 1920×1080) make images bigger than models accept. Keep the default
  1280×800, or pass `--image-limit standard` to `do`/`run` so screenshots and coordinates shrink
  together.
- Apps must speak **Wayland**; X11-only apps never show a window. Then `wait`/`start` times out, and
  the error carries `app_log_tail`.
- Read error codes, not just messages:
  - `app_exited`: the app crashed. Read `app_log_tail`, or `app.log` in the session directory.
  - `protocol_error`: the app's buffers were rejected; see `status` → `client_errors`.
  - `unmappable_text`: the keymap can't type that character. Nothing was typed.
  - `timeout`: still starting (raise `--timeout-ms`) or not a Wayland app.
- The session is isolated: a fresh `HOME`, a private D-Bus bus, no host display. Cargo, rustup and
  mise still find their toolchains. To use real user config, pass `--home host`.
- One app per session. For a second app, start another session (different `--id`).
- Clean up with `vway stop -s ID` (or `vway stop --all`). `vway run` cleans up by itself.
- If the host lacks something (no Vulkan driver, no lavapipe, no D-Bus), `vway doctor` names the
  missing package. Tell the user; don't install system packages yourself.

Every command has `--help`.
