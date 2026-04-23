#!/usr/bin/env bash
# Run the static-anthropic-server binary with a responses file.
#
# Usage:
#   ./run-static-anthropic-server.sh --response tests/responses.json
#   ./run-static-anthropic-server.sh --response /path/to/custom_responses.json --port 8888
#
# Defaults:
#   --response → tests/responses.json (relative to repo root)
#   --port     → 9876

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

DEFAULT_RESPONSES="${REPO_ROOT}/tests/responses.json"
PORT="9876"
RESPONSES=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --response)
            RESPONSES="$2"
            shift 2
            ;;
        --port)
            PORT="$2"
            shift 2
            ;;
        --help|-h)
            echo "Usage: $0 --response <path> [--port <port>]"
            echo ""
            echo "Options:"
            echo "  --response <path>  Path to the JSON responses file"
            echo "  --port <port>      Port to bind the server (default: 9876)"
            echo "  --help, -h         Show this help message"
            exit 0
            ;;
        *)
            echo "Unknown argument: $1"
            echo "Run '$0 --help' for usage."
            exit 1
            ;;
    esac
done

if [[ -z "${RESPONSES}" ]]; then
    RESPONSES="${DEFAULT_RESPONSES}"
fi

if [[ ! -f "${RESPONSES}" ]]; then
    echo "Error: responses file not found: ${RESPONSES}"
    exit 1
fi

echo "[run-static-server] responses: ${RESPONSES}"
echo "[run-static-server] port: ${PORT}"
echo "[run-static-server] starting server..."
echo ""

cd "${REPO_ROOT}"
exec cargo run --quiet --bin static-anthropic-server -- \
    --port "${PORT}" \
    --responses "${RESPONSES}"
