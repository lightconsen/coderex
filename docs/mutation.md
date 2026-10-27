# Mutation Testing

Mutation testing asks whether the test suite notices a deliberately broken
implementation. [`cargo-mutants`] changes one expression at a time (flips a
comparison, deletes a call, replaces a return value) and runs the tests. A
mutant that survives is a place where the tests are green but do not constrain
the behavior.

## Running

```bash
cargo install cargo-mutants --locked
scripts/mutation.sh permission-engine     # fastest useful scope
scripts/mutation.sh compact
scripts/mutation.sh all                   # every scoped module
```

`scripts/mutation.sh` disables debug info (`CARGO_PROFILE_TEST_DEBUG=0`) to
keep each mutant rebuild small, writes reports under `target/mutants/`, and
accepts extra `cargo-mutants` options before `--`:

```bash
MUTATION_TEST_FILTER=permission_engine::tests \
  scripts/mutation.sh permission-engine --baseline skip -j 4
```

`MUTANTS_IN_PLACE=1` mutates the working tree instead of a copy. The nightly
driver runs mutation only with `RUN_MUTATION=1` and reports findings without
failing the run unless `MUTATION_STRICT=1`.

## Baseline — 2026-10-28

| module | mutants | caught | timeout | unviable | missed | effective kill |
|---|---:|---:|---:|---:|---:|---:|
| `core/src/permissions.rs` | 142 | 132 | 0 | 10 | 0 | **100%** |
| `core/src/permission_engine.rs` | 66 | 47 | 5 | 14 | 0 | **100%** |
| `core/src/compact.rs` | 173 | 144 | 0 | 16 | 13 | **91.7%** |
| `tools/src/sandbox_shell.rs` | 10 | 7 | 0 | 2 | 1 | **87.5%** |
| `hooks/src/engine.rs` | 212 | 148 | 3 | 25 | 36 | **80.7%** |
| `mcp/src/channel.rs` | 51 | 36 | 8 | 4 | 3 | **93.6%** |

Effective kill counts caught and timeout as detected. A timeout means the
mutant made the tests hang rather than fail; it is still a detection, but those
paths should eventually get bounded assertions instead. Unviable mutants do
not compile and are excluded from the denominator.

All scoped modules meet the 80% target. The improvements came from tests added
in `5f85548`: permission subjects/exemptions, pending-request bookkeeping,
compaction branches and boundaries, hook labels/conditions/watchers, MCP
handler slots and stream state, and the planted-program guard.

## Surviving mutants

The remaining misses fall into three groups:

- **Platform-only code.** `sandbox_shell::finish_spawn` is a no-op on macOS and
  Linux; its Windows AppContainer/Job Object path needs a Windows runner.
- **No observable surface.** `announce_status`, `Debug` implementations and
  `warn_about_inert_hooks` only write logs, which the test process does not
  capture. Changing them cannot fail a test without adding log capture.
- **Equivalent or unreachable on this platform.** Constant arithmetic on
  limits that current tests never approach, `cancel_hook` (the tree is killed
  immediately afterwards either way), and compaction boundary arithmetic where
  two expressions produce the same list for every reachable input.

They are tracked here rather than silently excluded. Adding
`#[cfg_attr(test, mutants::skip)]` would require a `mutants` dev-dependency;
`scripts/mutation.sh` instead prints the report so a reviewer can decide.

## Triage

For every surviving mutant, choose one:

1. **Add an assertion** that fails when the behavior changes (preferred).
2. **Make the surface observable** (an event, a return value, a log capture).
3. **Document why it is equivalent** or platform-only, here.

Do not widen an `exclude_re` just to make the number green.

[`cargo-mutants`]: https://mutants.rs/
