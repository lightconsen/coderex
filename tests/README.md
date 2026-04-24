# CodeRex Test Strategy

This directory documents the three-tier testing strategy for CodeRex.

| Tier | What | Speed | Determinism | Command |
|------|------|-------|-------------|---------|
| **Unit** | Single module, isolated logic | < 1 s | 100% | `cargo test -p <crate>` |
| **Integration** | Cross-crate interactions | ~5 s | 100% | `cargo test --test <name>` |
| **E2E Mock** | Full CLI against mock server | ~20 s | 100% | See below |
| **E2E Live** | Full CLI against real Kimi API | ~2 min | Depends on LLM | See below |

---

## Unit Tests

Every crate has inline unit tests in `src/` files:

```bash
# Run all unit tests for a specific crate
cargo test -p coderex-core
cargo test -p coderex-tools
cargo test -p coderex-plugins

# Run all unit tests across the workspace
cargo test --workspace --lib
```

Example: `crates/core/src/types.rs` contains `#[cfg(test)]` blocks for `Message`
serde round-trips and `ToolCall` validation.

---

## Integration Tests

Integration tests live in each crate's `tests/` directory or at the workspace level.
They verify cross-module contracts without spawning the full CLI binary.

```bash
# Run integration tests for a specific crate
cargo test -p coderex-cli --test e2e_code_agent -- --ignored

# Run all integration tests in the workspace
cargo test --workspace --tests
```

---

## E2E Tests

E2E tests exercise the full `coderex` binary end-to-end, including the REPL,
tool dispatch, and LLM API integration. There are two variants:

### E2E with Mock Server (fast, deterministic, CI-friendly)

The `mock-anthropic-server` binary simulates Anthropic's streaming SSE API and
returns predetermined `tool_use` blocks. This allows testing tool execution
without calling a real LLM.

```bash
# 1. Start the mock server in a terminal
./target/debug/mock-anthropic-server --port 9876

# 2. In another terminal, source mock credentials and run tests
source export_env.local.sh
cargo test -p coderex-cli --test e2e_code_agent -- --ignored

# Or use the helper scripts
./tests/run-mock-anthropic-server.sh --port 9876
./tests/run-e2e-code-agent.sh
./tests/run-e2e-equivalence.sh
```

**What it tests:**
- Tool call assembly from SSE deltas (`Write`, `Edit`, `Bash`)
- Multi-turn conversation loops (tool result → next assistant message)
- File system effects (writing `src/lib.rs`, editing in place)
- Loop detection and budget limits
- Permission boundaries (dangerous commands rejected)

### E2E with Live Kimi API (slow, non-deterministic)

These tests run against the real Kimi API and verify CodeRex behaves correctly
with an actual LLM.

```bash
# 1. Source live API credentials
source export_env.kimi.sh

# 2. Run tests (single-threaded to avoid rate limits)
cargo test -p coderex-cli --test e2e_code_agent -- --ignored --test-threads=1

# Or use the helper script
./tests/run-e2e-code-agent.sh
```

**Requirements:**
- `ANTHROPIC_API_KEY` + `ANTHROPIC_BASE_URL` or `OPENAI_API_KEY` + `OPENAI_BASE_URL` must be set
- Tests are marked `#[ignore = "live LLM test"]` and require `--ignored`
- Single-threaded execution (`--test-threads=1`) to avoid rate limits

---

## Adding New Tests

### Unit Test

Add `#[cfg(test)]` at the bottom of the source file:

```rust
// crates/core/src/types.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_assistant_roundtrip() {
        let msg = Message::assistant("hello");
        assert!(matches!(msg.role, Role::Assistant));
        assert_eq!(msg.content_text(), Some("hello"));
    }
}
```

Run: `cargo test -p coderex-core message_assistant_roundtrip`

### Integration Test

Create `crates/<crate>/tests/<name>.rs`:

```rust
// crates/tools/tests/dispatcher.rs
use coderex_tools::{ToolRegistry, RegistryToolDispatcher};

#[tokio::test]
async fn dispatcher_routes_to_correct_tool() {
    let registry = ToolRegistry::new();
    let dispatcher = RegistryToolDispatcher::new(registry);
    // ... test cross-crate interaction
}
```

Run: `cargo test -p coderex-tools --test dispatcher`

### E2E Test

Add a test to `crates/cli/tests/e2e_code_agent.rs`:

```rust
#[test]
#[ignore = "live LLM test"]
fn my_new_scenario() {
    let tmp = tempdir();
    let out = run(tmp.path(), "my prompt here", 60);
    assert!(out.stdout.contains("expected output"));
}
```

For mock-server-only tests (no `#[ignore]` needed), add a separate test file
in `crates/cli/tests/` that uses `ANTHROPIC_API_KEY=mock`.

---

## CI Integration

Unit and integration tests run on every PR. E2E mock tests run on every PR.
E2E live tests run nightly or on demand.

```yaml
# Example CI step
- name: Unit + Integration Tests
  run: cargo test --workspace --lib --tests

- name: E2E Mock Tests
  run: |
    ./target/debug/mock-anthropic-server --port 9876 &
    source export_env.local.sh
    cargo test -p coderex-cli --test e2e_code_agent -- --ignored
```
