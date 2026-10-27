# CodeRex Test Strategy

CodeRex has three test tiers.

| Tier | What | Speed | Determinism | Command |
|------|------|-------|-------------|---------|
| **Unit** | Single module, isolated logic | < 1 s | 100% | `cargo test -p <crate>` |
| **Integration** | Cross-crate interactions | ~5 s | 100% | `cargo test --test <name>` |
| **E2E** | Full CLI / engine against a provider | ~2 min | Recorded replay | See below |

---

## Unit Tests

Every crate has inline unit tests in `src/` files:

```bash
cargo test -p coderex-core
cargo test --workspace --lib
```

## Integration Tests

Integration tests live in each crate's `tests/` directory and verify
cross-module contracts without spawning the full CLI binary.

```bash
cargo test -p coderex-core --test engine_integration
cargo test --workspace --tests
```

## E2E Tests and LLM Cassettes

The provider-backed suites (`e2e_code_agent`, `e2e_equivalence`,
`code_agent_benchmark`, the `live_*` tests) drive the real binary or engine and
need an Anthropic-compatible endpoint. They are **not** `#[ignore]`d: the test
harness starts `llm-cassette` automatically, and it replays the recordings
committed under `crates/cli/tests/cassettes/<suite>/`. No key, network or
external server is needed for a normal `cargo test` or CI run.

The suites are still available under the usual runner scripts:

```bash
./tests/run-e2e-code-agent.sh
./tests/run-e2e-equivalence.sh
./tests/run-e2e-full.sh
```

### How the recordings work

`llm-cassette` keys a request by a canonicalized body: the system prompt,
tool-result text, images, thinking blocks, `cache_control` markers, the model
name and volatile values (temp paths, timestamps, UUIDs) are removed, and the
tool list is sorted. The same test therefore matches its recording even though
its temporary directory, tool output and tool registration order differ.

### Re-recording

After changing a suite's prompts or assertions, refresh its recordings once
against a real endpoint:

```bash
scripts/record-cassettes.sh --fresh
```

The script reads the endpoint and key from `$CODEREX_SETTINGS`
(`~/.coderex/settings.json` by default), runs the suites with
`CODEREX_CASSETTE_MODE=record`, and never prints the key. Commit the updated
`crates/cli/tests/cassettes/` files.

If a replay cannot find a recording, the cassette returns HTTP 404 with a
`cassette_missing` error. When `CODEREX_CASSETTE_TRACE` is set, the unmatched
canonical request is also written to
`crates/cli/tests/cassettes/<suite>/_missing_<key>.json` for inspection; normal
runs do not create these files.

### Cassette metadata and maintenance

Every recording carries a `metadata` object:

```json
"metadata": {
  "recorded_at": "2026-10-27T08:00:00Z",
  "suite": "e2e_equivalence",
  "test": "tool_write_new_file",
  "endpoint_host": "api.example.com",
  "model": "deepseek-flash"
}
```

`endpoint_host` is the host only — never the base URL, query or key. `test`
comes from the Rust test thread name; older or in-process test harnesses may
leave it empty. Recordings made before the field existed can be backfilled:

```bash
scripts/record-cassettes.sh --backfill
```

`--backfill` takes `recorded_at` from the file's most recent git commit date
(falling back to mtime), `suite` from the directory and `model` from the
request. It marks such files `"backfilled": true` and cannot invent
`endpoint_host` or `test`.

`--check` validates the tree without modifying it:

```bash
scripts/record-cassettes.sh --check
CASSETTE_MAX_AGE_DAYS=180 CASSETTE_MAX_MB=30 CASSETTE_STRICT=1 \
  scripts/record-cassettes.sh --check
```

It reports malformed JSON, credential-like strings, missing metadata, stale
recordings, unmatched-request markers and suite/tree sizes. Malformed files and
credential-like strings always fail; the other findings are warnings, and
`CASSETTE_STRICT=1` turns them into failures (for CI).

`--prune` replays every suite with `CODEREX_CASSETTE_TRACE` set, collects the
keys each suite actually requested, and deletes `.json` files those suites did
not use, plus `_missing_*.json` markers. It refuses to prune a suite whose
replay failed. Use `--dry-run` to list the deletions first. Prune on the
platform that runs the full suite; a platform-gated test may use recordings a
different platform never requests.

The tree budget is 30 MiB by default (`CASSETTE_MAX_MB`). Large responses
should keep only the fields replay needs; the request side already stores the
canonical form.

### What the E2E suites cover

- Tool call assembly and multi-turn loops (`Write`, `Edit`, `Bash`, `Read`)
- File-system effects and permission boundaries
- Loop detection, budget limits and interrupts
- Resume: transcript projection, plans/activity and hook arming
- Wire protocol and stream-json frames
- The full TUI through a PTY (permissions, themes, signals, notifications)

---

## Adding New Tests

### Unit Test

Add `#[cfg(test)]` at the bottom of the source file and run
`cargo test -p coderex-core <name>`.

### Integration Test

Create `crates/<crate>/tests/<name>.rs` and run
`cargo test -p <crate> --test <name>`.

### Provider-backed E2E Test

Add the test to `crates/cli/tests/e2e_code_agent.rs` (or another suite with a
cassette). Use the suite's `run` helper; it points the child at the cassette
server. Record the new request once with `scripts/record-cassettes.sh
--fresh`, then commit the cassette.

---

## CI

The regular workspace test job runs the E2E suites too; the cassettes make
them deterministic and offline. There is no separate provider job and no
secret is needed in CI.
