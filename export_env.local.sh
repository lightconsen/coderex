#!/usr/bin/env bash

# Mock Anthropic API server for E2E testing.
# Source this file before running tests against the mock server:
#   source export_env.local.sh
#
# Note: Do NOT use "set -euo pipefail" here — this file is sourced into
# the current shell and strict mode would break prompt hooks.

export ANTHROPIC_API_KEY="mock"
export ANTHROPIC_BASE_URL="http://127.0.0.1:9876"
