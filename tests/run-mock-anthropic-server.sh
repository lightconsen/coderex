#!/usr/bin/env bash
set -euo pipefail

# Start the mock Anthropic API server for E2E testing.
# Usage:
#   ./tests/run-mock-anthropic-server.sh [PORT]
# Default port is 9876.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
PORT="${1:-9876}"

BINARY="${WORKSPACE_DIR}/target/debug/mock-anthropic-server"

if [[ ! -x "$BINARY" ]]; then
    echo "Binary not found, building..."
    cargo build --bin mock-anthropic-server
fi

echo "[mock-anthropic-server] Starting on port ${PORT}..."
exec "$BINARY" --port "$PORT"
