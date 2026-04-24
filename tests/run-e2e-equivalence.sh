#!/usr/bin/env bash
set -euo pipefail

# Run E2E equivalence tests against mock server or live LLM.
#
# Mock server (fast, deterministic):
#   source export_env.local.sh
#   ./tests/run-e2e-equivalence.sh
#
# Live Kimi API (slow, non-deterministic):
#   source export_env.kimi.sh
#   ./tests/run-e2e-equivalence.sh

# Check required environment variables
if { [ -z "${ANTHROPIC_API_KEY:-}" ] || [ -z "${ANTHROPIC_BASE_URL:-}" ]; } && { [ -z "${OPENAI_API_KEY:-}" ] || [ -z "${OPENAI_BASE_URL:-}" ]; }; then
    echo "Error: ANTHROPIC_API_KEY+ANTHROPIC_BASE_URL or OPENAI_API_KEY+OPENAI_BASE_URL must be set."
    echo "  For mock server tests: source export_env.local.sh"
    echo "  For live Kimi tests:   source export_env.kimi.sh"
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "${SCRIPT_DIR}/.."
MODEL="${ANTHROPIC_MODEL:-${OPENAI_MODEL:-default}}"
echo "Running e2e_equivalence tests with model: ${MODEL}"
echo ""

# If using mock server, start it automatically
if [ "${ANTHROPIC_API_KEY:-}" = "mock" ] || [ "${OPENAI_API_KEY:-}" = "mock" ]; then
    if ! curl -sf http://127.0.0.1:9876/v1/messages -X POST -d '{}' >/dev/null 2>&1; then
        echo "Mock server not running, starting it..."
        ./tests/run-mock-anthropic-server.sh &
        MOCK_PID=$!
        sleep 2
        trap 'kill $MOCK_PID 2>/dev/null' EXIT
    fi
fi

cargo test -p coderex-cli --test e2e_equivalence -- --ignored --test-threads=1 "$@"
