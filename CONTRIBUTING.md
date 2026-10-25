# Contributing to CodeRex

Thanks for helping. This file covers the build, the test suites and review
expectations. The project has a single integration branch, `main`; open a pull
request against it from a topic branch.

## Prerequisites

- Rust **1.97 or newer** (`rust-toolchain.toml` selects the stable channel).
- A C compiler and `pkg-config`: QuickJS (mods), ring and wasmtime build C
  code.
- **Linux**: `libdbus-1-dev`, `pkg-config` and `lld` (the workspace links with
  lld through `.cargo/config.toml`).
- **Windows**: the MSVC toolchain.
- Optional: `bubblewrap` on Linux for the filesystem sandbox (Landlock is used
  as the fallback); Seatbelt is built in on macOS.

## Build and test

```bash
cargo build --workspace
cargo test --workspace          # full suite, roughly 3.9k tests
cargo fmt --all --check
cargo clippy --workspace        # warnings are reported, not denied (see below)
```

`scripts/build.sh` wraps the build and checks the toolchain and platform
prerequisites first. It builds the release profile by default; useful options
are `--debug`, `--workspace`, `--clean` and `--run -- <args>`.

Some suites need extra setup:

- `cargo test -p coderex-cli --test interactive_e2e_tool` drives the TUI
  through a real PTY. Source `export_env.local.sh` first: it points at the
  repository's mock Anthropic server, so no API key is needed
  (`source export_env.local.sh`).
- The live-provider eval is opt-in and needs credentials; see
  [`docs/evals.md`](docs/evals.md). CI skips it when `ANTHROPIC_API_KEY` is
  absent.
- Platform-gated tests (`#[cfg(target_os = ...)]`) only run on their platform.
  CI runs the suite on Linux, Windows and macOS.

`cargo clippy --workspace` currently prints a backlog of pre-existing lints and
is **not** run with `-D warnings`; do not introduce new warnings in files you
touch. `cargo fmt --all --check` is required.

`scripts/publication-check.sh` scans tracked files and the history for
credentials and local-only files; it is a good check before sharing a branch.

## Mods and plugins

The in-process mod runtime lives in `crates/mods` ([`docs/mods.md`](docs/mods.md))
and the plugin system in `crates/plugins` ([`docs/plugins.md`](docs/plugins.md)).
Mods are user-installed JavaScript/TypeScript executed in-process on QuickJS;
tests that run them live beside the crates.

## Pull requests

- Keep a change focused; split unrelated work.
- Add tests for behaviour changes; the workspace suite must stay green.
- Use conventional commit subjects (`feat:`, `fix:`, `ci:`, `docs:`, …) and do
  not add attribution trailers.
- If the change touches `.github/workflows/`, say so in the description: the
  pusher needs a token with the `workflow` scope.
- CI also runs `cargo check --workspace --all-features`, so optional features
  (`keyring`, `hot-reload`, `wasm-runtime`) must compile.

## Security

Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).

## License

By contributing you agree that your contribution is licensed under the Apache
License 2.0 (see [LICENSE](LICENSE)).
