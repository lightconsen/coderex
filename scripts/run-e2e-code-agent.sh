#!/usr/bin/env bash
set -euo pipefail

# Load API credentials
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "${SCRIPT_DIR}/export_local_env.sh"

# Run E2E code-agent tests against real LLM
cd "${SCRIPT_DIR}/.."
echo "Running e2e_code_agent tests with model: ${CODEREX_MODEL}"
echo ""
cargo test -p coderex-cli --test e2e_code_agent -- --ignored --test-threads=1 "$@"
