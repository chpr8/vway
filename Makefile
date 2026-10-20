# vway developer tasks.
# A workspace-local CARGO_HOME keeps $HOME untouched (and works where it is read-only).
CARGO_HOME := $(CURDIR)/.cargo-home
export CARGO_HOME
# Some environments inject a test runner through this variable; the e2e tests need real processes.
CARGO := env -u CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER cargo

.PHONY: build release test unit e2e real-apps fmt clippy check

build:
	$(CARGO) build --workspace --bins --examples

release:
	$(CARGO) build --release -p vway

# Unit + end-to-end tests (e2e spawns real sessions with the bundled test client).
test: build
	$(CARGO) test --workspace

unit:
	$(CARGO) test --workspace --lib --bins

e2e: build
	$(CARGO) test -p vway --test e2e --test mcp --test agents

# Opt-in: foot, alacritty and vkcube from the host, when installed.
real-apps: build
	$(CARGO) test -p vway --test real_apps -- --ignored

fmt:
	$(CARGO) fmt --all

clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

check: fmt clippy test
