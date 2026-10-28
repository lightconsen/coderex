# Verification Checks

The checks that are not part of `cargo test`: dependency advisories, licence
policy, clippy and rustdoc. All of them run through one script so a local
machine, cron, a Linux runner or CI issue the same commands.

```bash
scripts/verify.sh                 # audit, deny, licenses, clippy, rustdoc
scripts/verify.sh audit deny      # one or more named checks
scripts/verify.sh --strict        # clippy/rustdoc warnings become failures
```

| Check | Command | Current state |
|---|---|---|
| `audit` | `cargo audit` with `.cargo/audit.toml` | **0** unignored advisories |
| `deny` | `cargo deny check` | advisories, bans, licences, sources all pass |
| `licenses` | `cargo deny check licenses` | passes; reports `nucleo-matcher` (MPL-2.0, accepted) |
| `clippy` | `cargo clippy --workspace --all-targets` | **830** warnings, 0 errors |
| `rustdoc` | `cargo doc --workspace --no-deps` | **62** warnings, 0 errors |

Logs and a summary go to `target/verify/`. `scripts/nightly.sh` runs
`verify` by default (`RUN_VERIFY=0` disables it).

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

## Clippy and rustdoc backlogs

Neither is enforced yet: `cargo clippy --workspace` and `cargo doc` run in CI
without `-D warnings`. `scripts/verify.sh` prints the counts (830 and 62 on
2026-10-28) and only fails with `--strict`. To restore the strict gates:

1. clear the warnings (or pin the toolchain so a new stable release does not
   silently add lints);
2. then add `-- -D warnings` to clippy and `RUSTDOCFLAGS="-D warnings"` to the
   docs job, and pass `--strict` to `scripts/verify.sh`.

Do not add `#[allow]` at module scope to make the count smaller; fix the lint
or record a reviewed exception next to it.
