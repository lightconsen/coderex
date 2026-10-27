#!/usr/bin/env bash
# Adversarial filesystem soak: a thread continuously retargets a symlink
# between an allowed root and an outside root while Read and directory-walk
# paths run through verified_fs. A successful read must only ever see the
# allowed content; open/walk failures are expected while the link points
# outside.
#
# Usage: scripts/race-soak.sh [iterations]
#
# The test is #[ignore]d in the normal suite so it does not slow every run.

set -euo pipefail

cd "$(dirname "$0")/.."
iterations="${1:-20000}"

echo "race soak: ${iterations} iterations"
RACE_SOAK_ITERATIONS="$iterations" \
    cargo test -p coderex-tools --lib symlink_and_rename_churn \
    -- --ignored --nocapture
