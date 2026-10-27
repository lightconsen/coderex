# Test Coverage

Code coverage is measured with [`cargo-llvm-cov`]. Run:

```bash
scripts/coverage.sh
```

The script runs `cargo llvm-cov --workspace --no-fail-fast --lcov` and writes
three artifacts under `target/coverage/` (or the directory passed as the last
argument):

- `lcov.info` — raw line coverage.
- `coverage.tsv` — per-crate hit/total lines.
- `summary.md` — the Markdown table below.

`scripts/coverage.sh --summary-only` regenerates the tables from an existing
`lcov.info` without rerunning the suite.

Test-only code under `crates/*/tests/`, `benches/` and `examples/` is excluded
from the denominator. Unit-test code inside `src/` is still instrumented (and
counted); treat the numbers as a relative signal, not an absolute one.

## Targets

Advisory targets: **80% lines** for `core`, `cli` and `tools`, and **65% lines**
for the other workspace crates. The script prints a warning for every crate
below its target but always exits 0, so coverage does not block local work.
CI integration and PR diffs are not enabled yet.

## Baseline — 2026-10-27

Workspace total: **110,490 / 127,015 lines = 87.0%**.

| crate | lines hit | lines | coverage |
|---|---:|---:|---:|
| agents | 2354 | 2605 | 90.4% |
| artifacts | 3115 | 3426 | 90.9% |
| bridge | 4811 | 5149 | 93.4% |
| cli | 15225 | 20978 | 72.6% |
| commands | 3969 | 4494 | 88.3% |
| config | 2680 | 2924 | 91.7% |
| core | 23009 | 25725 | 89.4% |
| events | 980 | 1061 | 92.4% |
| hooks | 3696 | 3945 | 93.7% |
| insights | 793 | 824 | 96.2% |
| mcp | 7921 | 9040 | 87.6% |
| memory | 3863 | 4092 | 94.4% |
| mods | 3147 | 3465 | 90.8% |
| other (evals and root sources) | 1209 | 1498 | 80.7% |
| plugins | 7611 | 8867 | 85.8% |
| server | 1559 | 1831 | 85.1% |
| telemetry | 4079 | 4314 | 94.6% |
| test-utils | 263 | 528 | 49.8% |
| tools | 11383 | 12799 | 88.9% |
| ui | 6774 | 7258 | 93.3% |
| workflow | 2049 | 2192 | 93.5% |

Below target at baseline:

- **`cli` 72.6%.** Two test-support binaries counted in the crate
  (`mock_anthropic_server.rs`, `static_anthropic_server.rs`) have no coverage of
  their own; excluding them raises the crate to 78.1%. The remaining gap is
  concentrated in `repl.rs` (72.2%), `orchestrator.rs` (76.9%),
  `interactive_e2e.rs` (68.6%) and `tmux.rs` (74.2%).
- **`test-utils` 49.8%.** Test infrastructure rather than product code; it is
  exercised indirectly by the suites that use it, which line coverage of its
  own tests does not measure.

Other low-coverage source modules (≥100 lines):

| file | coverage |
|---|---:|
| `tools/src/builtins/chrome/client.rs` | 4.1% |
| `tools/src/builtins/lsp_tool.rs` | 32.9% |
| `core/src/wire_adapter.rs` | 39.8% |
| `tools/src/builtins/web_search_tool.rs` | 52.4% |
| `core/src/wire_transport.rs` | 57.3% |
| `core/src/worktree.rs` | 64.2% |
| `cli/src/interactive_e2e.rs` | 68.6% |
| `cli/src/repl.rs` | 72.2% |
| `tools/src/tool.rs` | 72.8% |
| `commands/src/advanced_commands.rs` | 72.9% |

Coverage is not a correctness signal by itself. The safety-critical modules
(`permissions`, `permission_engine`, sandbox, hooks and compaction) are also
targeted by mutation testing and fuzzing; see the testing backlog.

[`cargo-llvm-cov`]: https://github.com/taiki-e/cargo-llvm-cov
