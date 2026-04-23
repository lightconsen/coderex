#!/usr/bin/env bash
set -euo pipefail

# Check required environment variables
if [ -z "${CODEREX_API_KEY:-}" ] || [ -z "${CODEREX_BASE_URL:-}" ]; then
    echo "Error: CODEREX_API_KEY and CODEREX_BASE_URL must be set."
    echo "  For mock server tests: source export_env.local.sh"
    echo "  For live Kimi tests:   source export_env.kimi.sh"
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "${SCRIPT_DIR}/.."
echo "Running e2e_code_agent tests with model: ${CODEREX_MODEL:-default}"
echo ""
cargo test -p coderex-cli --test e2e_code_agent -- --ignored --test-threads=1 "$@"
