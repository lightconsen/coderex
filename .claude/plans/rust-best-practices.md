# Rust Industry Best Practices for CodeRex

## Context

CodeRex is a large-scale Rust project (~25 phases, 15+ crates) reimplementing Claude Code. Without explicit practices, the project risks:
- Unbounded compile times (30+ min in large monoliths)
- Dependency version drift across crates
- Inconsistent error handling patterns
- Missing supply chain security
- Poor IDE experience for contributors

This plan covers 16 categories of industry best practices applicable to CodeRex.

---

## 1. Workspace Architecture (Cargo Workspace)

**What to do:**

- All crates under `crates/` directory, not flat
- Root `Cargo.toml` declares workspace members
- Centralized `[workspace.dependencies]` — all shared deps versioned here
- `resolver = "2"` for feature resolution

```toml
# Cargo.toml (root)
[workspace]
members = ["crates/*"]
resolver = "2"

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
reqwest = { version = "0.12", features = ["json", "stream"] }
tracing = "0.1"
thiserror = "2"
anyhow = "1"
ratatui = "0.29"
crossterm = "0.28"
clap = { version = "4", features = ["derive"] }
sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite"] }
jsonschema = "0.28"
# ... all shared deps
```

Each crate then uses:
```toml
[dependencies]
tokio.workspace = true
serde.workspace = true
thiserror.workspace = true
```

---

## 2. Compile Time Optimization

**Why critical:** Large workspaces can hit 20-30 min compile times.

**Practices:**
- Feature flags on heavy deps (e.g., `reqwest` only in crates that need HTTP)
- Avoid re-exporting large crates through public API
- Use `cargo build -p <crate>` for focused builds
- Profile with `cargo build --timings` regularly
- Precompiled headers: use `sccache` in CI
- LLD linker: configure in `.cargo/config.toml`

```toml
# .cargo/config.toml
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=lld"]

[target.aarch64-apple-darwin]
rustflags = ["-C", "link-arg=-fuse-ld=lld"]

[build]
rustflags = ["-Z", "share-generics=y"]  # nightly only
```

---

## 3. Error Handling

**Pattern:**
- `thiserror` for library errors (each crate defines its own `Error` enum)
- `anyhow` only in binary crate (`cli`) for application-level errors
- Never use `.unwrap()` or `.expect()` in library code — use `?` with proper error conversion
- Context via `.context()` for operational errors

```rust
// In each library crate:
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("tool '{name}' not found")]
    NotFound { name: String },
    #[error("execution timeout after {timeout:?}")]
    Timeout { timeout: Duration },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
```

---

## 4. Code Quality & Lints

**Enforce at workspace level:**

```toml
# Cargo.toml (root)
[workspace.lints.rust]
unsafe_code = "deny"
unused = "warn"
missing_debug_implementations = "warn"

[workspace.lints.clippy]
all = { level = "warn", priority = -1 }
pedantic = { level = "warn", priority = -1 }
nursery = { level = "warn", priority = -1 }
# Allow specific rules that are too noisy
must_use_candidate = "allow"
missing_errors_doc = "allow"
```

Each crate inherits:
```toml
[lints]
workspace = true
```

---

## 5. Testing Strategy

**Three layers:**
1. **Unit tests** — in `mod tests { }` at bottom of each module
2. **Integration tests** — in `tests/` directory per crate
3. **E2E tests** — in top-level `tests/e2e/`

**Tooling:**
- `cargo-nextest` for faster, parallel test execution
- `tempfile` for filesystem tests
- `mockall` for mock generation
- `wiremock` for HTTP mock server
- `proptest` for property-based testing on complex invariants (budget tracking, state machine transitions)

---

## 6. Supply Chain Security

**Practices:**
- `cargo-audit` in CI — check for known CVEs
- `cargo-vet` for dependency auditing (Mozilla's tool)
- `cargo-deny` for license compliance + duplicate dependency detection
- Pin critical dependency versions, avoid `*` or loose ranges
- Minimal dependency surface — audit each new `Cargo.toml` entry

```toml
# deny.toml
[licenses]
unlicensed = "deny"
allow = ["MIT", "Apache-2.0", "BSD-3-Clause", "ISC", "Unicode-3.0"]

[bans]
multiple-versions = "warn"
wildcards = "deny"
```

---

## 7. Async Patterns (tokio)

**Practices for CodeRex:**
- Single `#[tokio::main]` entry point, pass `Runtime` handle explicitly
- Never `block_on()` inside async code
- Use `tokio::select!` for concurrent event consumption (as spec already describes)
- Use `tokio::sync::watch` for shared state (AppState, settings)
- Use `tokio::sync::mpsc` for bounded channels between subsystems
- Use `tokio::sync::oneshot` for request-response patterns
- `JoinSet` for parallel task execution
- `JoinHandle` with `.abort()` for cancellable operations

```rust
// Shared config via watch channel
let (settings_tx, settings_rx) = watch::channel(Settings::default());
// Consumers: let settings = settings_rx.borrow().clone();
// Updaters: settings_tx.send_replace(new_settings);
```

---

## 8. Type System Patterns

**Practices:**
- **Newtype pattern** for domain types (prevent mixing similar primitives):
  ```rust
  pub struct SessionId(Uuid);
  pub struct ProjectHash(String);
  pub struct CostUsd(f64);
  ```
- **Builder pattern** for complex structs (use `derive_builder` crate)
- **State types** to encode valid transitions at compile time (TurnState enum as already in spec)
- **`#[non_exhaustive]`** on public enums (allow future variants without semver break)
- **`#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]`** on all data types

---

## 9. Documentation

**Practices:**
- `cargo doc --no-deps` in CI — ensure docs build
- Doc comments on all `pub` items
- `#![warn(missing_docs)]` on library crates
- Examples in doc comments for complex APIs
- Architecture Decision Records (ADRs) in `docs/adr/`
- `README.md` per crate explaining purpose

---

## 10. CI/CD Pipeline

**Minimum CI gates:**
```yaml
# .github/workflows/ci.yml
jobs:
  build: cargo build --workspace
  test: cargo nextest run --workspace
  clippy: cargo clippy --workspace -- -D warnings
  fmt: cargo fmt --check
  audit: cargo audit
  deny: cargo deny check
  doc: cargo doc --workspace --no-deps
```

Plus:
- Cross-platform build matrix: Linux, macOS, Windows
- `cargo msrv` to track minimum supported Rust version

---

## 11. Release Engineering

**Practices:**
- `cargo-release` for version management
- Semantic versioning across all workspace crates
- `CHANGELOG.md` with conventional commits format
- Feature flags for `core`, `full`, `enterprise` tiers (as in spec 24.x)
- `--features` gating for platform-specific code

---

## 12. Memory & Performance

**Practices:**
- `#[inline]` on hot-path small functions
- `String` vs `&str` — prefer `&str` in API, `String` in owned data
- `Cow<str>` for read-mostly text that sometimes needs mutation
- `Arc<str>` instead of `Arc<String>` for shared strings
- `bytes::Bytes` for large binary blobs (transcripts, API responses)
- Avoid `.clone()` on large data structures — use `Arc` instead
- `smallvec` for small collections that rarely exceed capacity
- `compact_str` for short strings (tool names, command names)

---

## 13. Configuration Management

**Practices:**
- `config` crate for settings loading/merging (already in spec)
- Environment variables via `std::env::var` or `dotenvy` for dev
- `CLAUDE_CONFIG_DIR` env override (as in spec)
- Validate config at load time, not at use time
- Use `serde(default)` for optional fields

---

## 14. Git Hooks & Pre-commit

```toml
# Cargo.toml dev-dependencies
[workspace.metadata.hooks]
pre-commit = ["cargo fmt --check", "cargo clippy --workspace"]
```

Use `cargo-husky` or `.git/hooks/pre-commit` for:
- `cargo fmt --check`
- `cargo clippy --workspace -- -D warnings`
- `cargo test --workspace`

---

## 15. Logging & Observability

**Practices (already aligned with spec):**
- `tracing` for structured logging (not `log`)
- `tracing-subscriber` with JSON format for production, pretty for dev
- `tracing` spans for each turn, tool call, API request
- `tracing` fields for correlation IDs
- `metrics` crate for counters/gauges (cost, tokens, latency)
- `console-subscriber` for tokio task debugging in dev

---

## 16. Development Tooling

**Recommended tools for contributors:**
- `cargo-watch` — `cargo watch -x 'clippy --workspace'` for auto-rebuild
- `cargo-expand` — inspect macro expansion
- `cargo-udeps` — find unused dependencies
- `cargo-machete` — faster unused dependency checker
- `typos-cli` — catch typos in docs/code
- `cargo-fuzz` — fuzz testing for parsers (tool call JSON, hook output)

---

## Summary: What to Add to Phase 0.1

These practices should be codified in Phase 0.1 (project skeleton):

| Practice | Where |
|----------|-------|
| Workspace `crates/` layout | `Cargo.toml` root |
| Centralized dependencies | `[workspace.dependencies]` |
| Lint configuration | `[workspace.lints]` + `clippy.toml` |
| `.cargo/config.toml` | LLD linker, sccache |
| CI workflow | `.github/workflows/ci.yml` |
| Supply chain checks | `deny.toml`, `audit` job |
| Error handling pattern | `thiserror` in workspace deps |
| `Cargo.toml` template per crate | Common dev-dependencies, lints |
| `rust-toolchain.toml` | Stable channel with edition 2024 |
| `README.md` per crate | Purpose, examples |
