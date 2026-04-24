#!/usr/bin/env bash
set -euo pipefail

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
echo "Running e2e_code_agent tests with model: ${MODEL}"
echo ""
cargo test -p coderex-cli --test e2e_code_agent -- --ignored --test-threads=1 "$@"
