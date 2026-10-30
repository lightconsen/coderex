# Verification Checks

The checks that are not part of `cargo test`: dependency advisories, licence
policy, clippy and rustdoc. All of them run through one script so a local
machine, cron, a Linux runner or CI issue the same commands.

```bash
scripts/verify.sh                 # audit, deny, licenses, clippy, rustdoc
scripts/verify.sh audit deny      # one or more named checks
scripts/verify.sh --lenient       # report clippy/rustdoc counts without failing
```

| Check | Command | Current state |
|---|---|---|
| `audit` | `cargo audit` with `.cargo/audit.toml` | **0** unignored advisories |
| `deny` | `cargo deny check` | advisories, bans, licences, sources all pass |
| `licenses` | `cargo deny check licenses` | passes; reports `nucleo-matcher` (MPL-2.0, accepted) |
| `clippy` | `cargo clippy --workspace --all-targets` | **0** warnings, 0 errors |
| `rustdoc` | `cargo doc --workspace --no-deps` | **0** warnings, 0 errors |

Logs and a summary go to `target/verify/`. `scripts/nightly.sh` runs
`verify` by default (`RUN_VERIFY=0` disables it).

`cargo test -p coderex-cli --test real_session_corpus` replays real recorded
sessions through compaction, loop detection and the transcript store when the
machine has them (`~/.claude/projects`, or `CODEREX_REAL_SESSIONS_DIR`); without
a corpus it reports that and passes, which is what CI does. See
`tests/README.md`.

## The CI gate

`.github/workflows/ci.yml` runs the same checks on every push and pull
request (branch protection decides which of them must pass before a merge;
this table is what they check):

| Job | What it enforces |
|---|---|
| `fmt` | `cargo fmt --all --check` |
| `cassettes` | `python3 scripts/cassette_tool.py check --strict` (metadata, size, staleness, credentials; runs in seconds) |
| `ssh-integration` | a throwaway `sshd` on a high port, then `cargo test -p coderex-bridge --test ssh_spawn` — the SSH spawn path against a real server |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings` (the backlog is zero, so a warning fails) |
| `test` | `cargo nextest run --workspace --no-fail-fast` on Linux; the macOS plist carrier and the live benchmark replay run in `test-macos`, the Windows runtime sandbox tests in `test-windows`, and the SSH spawn path in `ssh-integration` |
| `coverage` | `scripts/coverage.sh`; fails below an 85% workspace floor, uploads `coverage-lcov`, and comments the total and delta on pull requests |
| `doc` | `cargo doc` with `RUSTDOCFLAGS="-D warnings"` |
| `audit` / `deny` | `cargo audit`, `cargo deny check licenses bans sources` |
| `Nightly Soak & Fuzz` | scheduled: `scripts/soak.py`, both race soaks, `scripts/fuzz.sh` |
| `Release` workflow `smoke` | the published archives unpack to a binary that runs (`--version`) and contain the file they are named for |

The toolchain is pinned in `rust-toolchain.toml` and named in the workflow
files. Bump both together: a floating stable would let a new release's lints
fail a pull request for reasons unrelated to the change.

## Advisories

`.cargo/audit.toml` (cargo-audit's fixed location) and `deny.toml` carry the
same reviewed ignore list. Update both together.

| Advisory | Crate | Why it is ignored |
|---|---|---|
| RUSTSEC-2024-0320 | `yaml-rust` | optional syntect feature; not compiled, but Cargo.lock records it |
| RUSTSEC-2025-0141 | `bincode` | syntect's dump format; unmaintained, no known vulnerability |
| RUSTSEC-2026-0370 | `zvariant` | zbus 5 stack used by keyring's Secret Service store; no patched version |
| RUSTSEC-2026-0371 | `zbus_names` | same stack |
| RUSTSEC-2026-0372 | `zvariant_derive` | same stack |

`zstd` (RUSTSEC-2026-0391/0398/0403/0410) was removed by disabling wasmtime's
default `cache` feature: CodeRex never configures a wasmtime cache, so the
dependency was unnecessary.

Both checks fall back to the cached advisory database when GitHub is
unreachable, and say so in the log; a *finding* still fails the run.

## Licence policy

`deny.toml` allows `MPL-2.0` for `nucleo-matcher` (file-level weak copyleft,
used unmodified). **Accepted by decision on 2026-10-28.** The obligation is to
publish modifications to MPL-covered files; there are none, and the crate is
used as a normal Cargo dependency. GPL/AGPL remain denied.

`scripts/verify.sh licenses` prints the MPL-2.0 dependency each run so the
allow entry cannot silently widen to another crate; adding a second MPL
dependency is a policy decision, not a config edit.

## Clippy and rustdoc

Both are clean, and `scripts/verify.sh` now fails on any warning. The
workspace lints in `Cargo.toml` allow a small, documented set of lints whose
suggested rewrite is worse than the code (`assert_is_empty`,
`significant_drop_tightening`, `await_holding_lock`, `option_if_let_else`,
`cast_possible_truncation`, `type_complexity`, `needless_pass_by_value`).

## Keeping it clean

`scripts/verify.sh` fails on a clippy or rustdoc warning, so a new one is a
regression rather than a number nobody reads. Two things are deliberate:

- A small set of lints is allowed at the workspace level for a recorded
  reason (see the comment in `Cargo.toml`). Each is a lint whose suggested
  rewrite is worse than the code it replaces, not a warnings count being
  hidden.
- A site-specific `#[allow]` carries a comment saying why the lint does not
  apply there (a `#[non_exhaustive]` struct, a shell string that looks like a
  format argument, a lock held across an await in a test).

Do not add `#[allow]` at module scope to make the count smaller; fix the lint
or record a reviewed exception next to it.
