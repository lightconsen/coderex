#!/usr/bin/env bash
set -euo pipefail

# Mock Anthropic API server for E2E testing.
# Source this file before running tests against the mock server:
#   source export_env.local.sh

export CODEREX_API_KEY="mock"
export CODEREX_BASE_URL="http://127.0.0.1:9876"
