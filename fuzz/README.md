# Fuzz targets

These targets run under [`cargo-fuzz`] and the nightly toolchain. The
repository pins stable in `rust-toolchain.toml`, so invoke them through
`scripts/fuzz.sh` (which uses `cargo +nightly fuzz`) or explicitly:

```bash
cargo +nightly fuzz list
cargo +nightly fuzz run fuzz_shell_ast -- -max_total_time=60
```

| Target | Input | Invariant |
|---|---|---|
| `fuzz_sse_stream` | arbitrary bytes | UTF-8 chunk decoding never panics; SSE framing always advances or terminates |
| `fuzz_wire_protocol` | arbitrary line | decoded `WireEvent`/`WireRequest` re-encode into a line the parser accepts; unknown input errors instead of panicking |
| `fuzz_settings_json` | arbitrary JSON | `Settings` parse + `validate` + re-serialize never panics; malformed input is rejected |
| `fuzz_plugin_manifest` | arbitrary JSON | `PluginManifest` (which denies unknown fields) parses or errors without panicking |
| `fuzz_shell_ast` | arbitrary command | `analyze_command` is deterministic and never returns an empty segment |

Seeds live in `corpus/<target>/seed.txt`. libFuzzer's generated corpus entries
(SHA-1 names) and `target/`, `artifacts/` are gitignored; commit a minimized
crash input as a regression test, not as a corpus blob.

Local smoke: `scripts/fuzz.sh 30` (30 seconds per target). Nightly:
`RUN_FUZZ=1 FUZZ_SECONDS=600 scripts/nightly.sh`.

[`cargo-fuzz`]: https://github.com/rust-fuzz/cargo-fuzz
