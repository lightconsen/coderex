#!/usr/bin/env bash
# Run every E2E suite. The provider-backed suites replay the committed
# cassettes, so no key or network is required.
#
# Re-record the cassettes (once, against a real endpoint) with:
#   scripts/record-cassettes.sh --fresh
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

failed=0
run() {
  echo "== $*"
  if ! cargo test -p coderex-cli "$@" -- --test-threads=1; then
    failed=1
  fi
}

run --test e2e_core_flows
run --test e2e_stress
run --test e2e_engine_flows
run --test live_basic_chat
run --test live_bash_tool
run --test live_file_read
run --test e2e_code_agent
run --test e2e_equivalence
run --test code_agent_benchmark

exit "$failed"
