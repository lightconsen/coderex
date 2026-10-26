#!/usr/bin/env bash
# Run the equivalence E2E suite against the recorded LLM cassettes.
# No provider key or network is needed; re-record with
# scripts/record-cassettes.sh when the suite changes.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
cargo test -p coderex-cli --test e2e_equivalence -- --test-threads=1 "$@"
