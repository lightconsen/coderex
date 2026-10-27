#!/usr/bin/env bash
# Run the cargo-fuzz targets for a bounded time.
#
# Usage: scripts/fuzz.sh [seconds-per-target] [target ...]
#
# Defaults to 30 seconds per target and every target. The nightly toolchain is
# required for libFuzzer instrumentation; the repository's rust-toolchain.toml
# pins stable, so the script invokes `cargo +nightly fuzz`.
#
# Examples:
#   scripts/fuzz.sh                 # 30s each, seed corpus
#   scripts/fuzz.sh 300             # 5 minutes each
#   scripts/fuzz.sh 60 fuzz_shell_ast
#
# Findings are printed by libFuzzer and stored under fuzz/artifacts/<target>/.
# Committed seeds live in fuzz/corpus/<target>/seed.txt; libFuzzer's generated
# corpus entries are gitignored.

set -euo pipefail

cd "$(dirname "$0")/.."

seconds="${1:-30}"
if [[ $# -gt 0 ]]; then
    shift
fi

if [[ $# -gt 0 ]]; then
    targets=("$@")
else
    targets=(
        fuzz_sse_stream
        fuzz_wire_protocol
        fuzz_settings_json
        fuzz_plugin_manifest
        fuzz_shell_ast
    )
fi

command -v cargo-fuzz >/dev/null 2>&1 || {
    echo "cargo-fuzz is not installed; run: cargo install cargo-fuzz --locked" >&2
    exit 1
}

rustup toolchain list | grep -q '^nightly' || {
    echo "the nightly toolchain is required: rustup toolchain install nightly" >&2
    exit 1
}

echo "fuzzing ${#targets[@]} target(s) for ${seconds}s each"
echo

status=0
for target in "${targets[@]}"; do
    echo "== $target"
    if ! cargo +nightly fuzz run "$target" -- \
        -max_total_time="$seconds" \
        -print_final_stats=1; then
        echo "fuzz target failed: $target" >&2
        status=1
    fi
    echo
done

if [[ -d fuzz/artifacts ]]; then
    artifacts=$(find fuzz/artifacts -type f ! -name '*.log' 2>/dev/null || true)
    if [[ -n "$artifacts" ]]; then
        echo "crash inputs:" >&2
        printf '  %s\n' $artifacts >&2
    fi
fi

exit "$status"
