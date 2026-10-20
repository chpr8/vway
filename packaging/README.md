# Packaging

vway is Linux-only. Every channel installs one binary, `vway`. No channel writes to the user's home;
users run `vway agents setup` themselves to connect their coding agents.

| Channel | Install | Built from |
|---|---|---|
| crates.io | `cargo install --locked vway` | source (needs `pkg-config`, libxkbcommon and pixman headers) |
| mise | `mise use -g github:chpr8/vway` or `mise use -g cargo:vway` | release archive / crates.io |
| Claude Code plugin | `/plugin marketplace add chpr8/vway`, then `/plugin install vway@vway` | the repository root (skill and MCP server only; the binary comes from a channel above) |
| Other agents | `npx skills add chpr8/vway` | `skills/vway/` (skill only) |

Compiling for macOS or Windows stops with one `compile_error!` that says vway is Linux-only.

## Releasing X.Y.Z

1. Set `version` in `Cargo.toml` and `.claude-plugin/plugin.json`. A unit test checks that they
   match.
2. `make check`, then tag `vX.Y.Z` and push the tag. `.github/workflows/release.yml` builds
   `vway-vX.Y.Z-{x86_64,aarch64}-unknown-linux-gnu.tar.gz` on Ubuntu 22.04 (glibc 2.35) and
   creates the GitHub release.
3. `cargo publish -p vway-proto`, then `-p vway-compositor`, then `-p vway`.

## mise registry

Until vway is in the mise registry, users name the backend (`github:chpr8/vway`). The registry
entry would be:

```toml
[tools.vway]
description = "Headless Wayland desktops for testing and driving GUI apps"
backends = ["github:chpr8/vway", "cargo:vway"]
os = ["linux"]
test = ["vway --version", "vway {{version}}"]
```

## Runtime dependencies

These are linked at build time, so packages must depend on them: `libxkbcommon` (with
xkeyboard-config data) and `pixman`. Everything else is optional and only affects what can run
inside a session: D-Bus (`dbus-daemon`), a GPU render node with Mesa EGL, the Vulkan loader, and
Mesa's lavapipe for `--gpu software`. `vway doctor` lists what is missing, with the install command
for the detected distribution.
