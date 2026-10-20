---
name: vway
description: >-
  Run a GUI app on a private headless Wayland desktop and look at it or use it: screenshots,
  clicks, typing, key combos, scrolling, touch gestures, resizing and fractional scaling, without
  touching the user's screen or input. Use when you need to see or interact with a GUI app you are
  building (GPUI, GTK, Qt, winit/wgpu, Electron on Wayland, terminals), reproduce a UI bug, or verify
  a visual change. Linux only.
---

# vway

`vway` runs one GUI app on a private headless Wayland desktop and gives you its pixels plus a real
keyboard, pointer and touch seat. Nothing reaches the user's screen or input.

**Run `vway guide` before your first use and read it.** It is the reference for the vway version
installed here: modes, flags, rules and error codes.

```console
vway run --screenshot shot.png -- ./target/debug/my-app   # one shot: start, screenshot, clean up
vway start --id app -- cargo run                           # a session for several steps
vway screenshot -s app -o shot.png
vway click -s app 200,300
vway type -s app "hello"
vway key -s app ctrl+s
vway stop -s app
```

Every command prints one JSON document with `"ok"` first. Coordinates are logical pixels, while
screenshots are physical pixels: divide what you read off the image by the scale.

If `vway` is not found, it is not installed. Ask the user to install it (`cargo install vway` or
mise) rather than installing it yourself.
