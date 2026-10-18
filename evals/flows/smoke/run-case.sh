#!/usr/bin/env bash
# Run one prompt against a local, static Anthropic fixture.
#
# The eval harness passes the case prompt as $1. The fixture server is the
# repository's own static-anthropic-server, fed tests/responses.json; no
# network and no API key are involved.
set -euo pipefail

PROMPT="${1:-hello}"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../../.." && pwd)
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
CODEREX_BIN="${CODEREX_BIN:-$TARGET_DIR/debug/coderex}"
SERVER_BIN="${STATIC_ANTHROPIC_SERVER_BIN:-$TARGET_DIR/debug/static-anthropic-server}"
RESPONSES="${CODEREX_EVAL_RESPONSES:-$REPO_ROOT/tests/responses.json}"

if [[ ! -x "$CODEREX_BIN" || ! -x "$SERVER_BIN" ]]; then
    cargo build --quiet \
        --manifest-path "$REPO_ROOT/Cargo.toml" \
        -p coderex-cli --bin coderex --bin static-anthropic-server
fi

log=$(mktemp)
server_pid=""
cleanup() {
    if [[ -n "$server_pid" ]]; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    rm -f "$log"
}
trap cleanup EXIT

"$SERVER_BIN" --port 0 --responses "$RESPONSES" >"$log" 2>&1 &
server_pid=$!

origin=""
for _ in $(seq 1 200); do
    origin=$(sed -n 's/.*listening on \([^ ]*\).*/\1/p' "$log" | head -n1)
    if [[ -n "$origin" ]]; then
        break
    fi
    if ! kill -0 "$server_pid" 2>/dev/null; then
        echo "static-anthropic-server exited before it listened:" >&2
        cat "$log" >&2
        exit 1
    fi
    sleep 0.05
done

if [[ -z "$origin" ]]; then
    echo "timed out waiting for static-anthropic-server:" >&2
    cat "$log" >&2
    exit 1
fi

# The child inherits the rest of the environment deliberately: this is the
# same binary a person runs. Only the provider endpoint is redirected.
ANTHROPIC_API_KEY=mock \
ANTHROPIC_BASE_URL="http://$origin" \
CODEREX_SKIP_ENV_SOURCE=1 \
"$CODEREX_BIN" --print --brief "$PROMPT"
