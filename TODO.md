# TODO

## Next

- [ ] Draw the cursor in screenshots (optional, `--cursor`); honour `wp_cursor_shape` and cursor surfaces.
- [ ] Clipboard from the CLI (`vway clipboard get|set`): the compositor already serves `wl_data_device`
      and primary selection between clients, but nothing exposes them.
- [ ] Text input: `zwp_text_input_v3` / input-method so IME-aware toolkits get composed text.
- [ ] `xdg-decoration` (tell clients to draw their own), `xdg-activation`, `wp_cursor_shape`,
      `wp_presentation` (some toolkits pace frames with it).
- [ ] Multiple outputs, and per-output scale.
- [ ] Damage-tracked capture: reuse one damage tracker and offscreen buffer between screenshots.
- [ ] Screenshot diffing helpers (`vway screenshot --compare ref.png --threshold`).
- [ ] Warm session pool for the MCP server (`--pool N`) to hide startup latency.
- [ ] `vway-test` crate for `[dev-dependencies]`: typed Rust API over the CLI that finds the binary
      (`$VWAY_BIN`, `.tools/bin`, `PATH`) and checks the protocol version.
- [ ] GPUI example app under `examples/` with a smoke test (own workspace; heavy build).

## Later

- [ ] Recording (video) via the existing on-demand renderer at a fixed frame rate.
- [ ] Web: launch a browser in a session (`vway web --url`) with per-session profiles.
- [ ] Static pixman/xkbcommon (vendored C builds) for a binary with no library dependencies beyond
      glibc; Mesa vendoring decision for `--gpu software`.
- [ ] mise registry entry once the first release is tagged (see `packaging/`).
- [ ] User-namespace isolation tier (network off, read-only host).
