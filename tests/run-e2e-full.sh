#!/usr/bin/env bash
set -euo pipefail

# Run all E2E tests — both mock-server and live LLM variants.
#
# Mock server (fast, deterministic):
#   source export_env.local.sh
#   ./tests/run-e2e-full.sh
#
# Live Kimi API (slow, non-deterministic):
#   source export_env.kimi.sh
#   ./tests/run-e2e-full.sh

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
API_KEY_PREFIX="${ANTHROPIC_API_KEY:-${OPENAI_API_KEY:-}}"
echo "========================================"
echo "Running all E2E tests"
echo "Model: ${MODEL}"
echo "API key: ${API_KEY_PREFIX:0:4}..."
echo "========================================"
echo ""

# If using mock server, start it automatically
MOCK_PID=""
if [ "${ANTHROPIC_API_KEY:-}" = "mock" ] || [ "${OPENAI_API_KEY:-}" = "mock" ]; then
    if ! curl -sf http://127.0.0.1:9876/v1/messages -X POST -d '{}' >/dev/null 2>&1; then
        echo "Mock server not running, starting it..."
        ./tests/run-mock-anthropic-server.sh &
        MOCK_PID=$!
        sleep 2
    else
        echo "Mock server already running on port 9876"
    fi
fi

# Cleanup mock server on exit
cleanup() {
    if [ -n "${MOCK_PID}" ]; then
        kill "${MOCK_PID}" 2>/dev/null || true
        wait "${MOCK_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

FAILED=0

# ─── Tier 1: Non-ignored E2E tests (always run) ─────────────────────────────
echo "--- Tier 1: Core flows ---"
if ! cargo test -p coderex-cli --test e2e_core_flows "$@"; then
    FAILED=1
fi
echo ""

echo "--- Tier 2: Stress tests ---"
if ! cargo test -p coderex-cli --test e2e_stress "$@"; then
    FAILED=1
fi
echo ""

# ─── Tier 3: Ignored E2E tests (require --ignored, single-threaded) ────────
echo "--- Tier 3: Code agent tests (live LLM / mock) ---"
if ! cargo test -p coderex-cli --test e2e_code_agent -- --ignored --test-threads=1 "$@"; then
    FAILED=1
fi
echo ""

echo "--- Tier 4: Equivalence tests (live LLM / mock) ---"
if ! cargo test -p coderex-cli --test e2e_equivalence -- --ignored --test-threads=1 "$@"; then
    FAILED=1
fi
echo ""

# ─── Summary ────────────────────────────────────────────────────────────────
echo "========================================"
if [ "${FAILED}" -eq 0 ]; then
    echo "All E2E tests passed."
else
    echo "Some E2E tests FAILED."
fi
echo "========================================"

exit "${FAILED}"
