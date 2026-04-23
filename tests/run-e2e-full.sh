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
if [ -z "${CODEREX_API_KEY:-}" ] || [ -z "${CODEREX_BASE_URL:-}" ]; then
    echo "Error: CODEREX_API_KEY and CODEREX_BASE_URL must be set."
    echo "  For mock server tests: source export_env.local.sh"
    echo "  For live Kimi tests:   source export_env.kimi.sh"
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "${SCRIPT_DIR}/.."

echo "========================================"
echo "Running all E2E tests"
echo "Model: ${CODEREX_MODEL:-default}"
echo "API key: ${CODEREX_API_KEY:0:4}..."
echo "========================================"
echo ""

# If using mock server, start it automatically
MOCK_PID=""
if [ "${CODEREX_API_KEY}" = "mock" ]; then
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
