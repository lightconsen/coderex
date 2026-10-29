#!/usr/bin/env bash
# Multi-process adversarial filesystem soak.
#
# `scripts/race-soak.sh` runs one process: one attacker thread against one
# verifier. This runs the same soak as several concurrent processes, so it
# also covers what a single process cannot — global state (environment,
# temp-file naming, the OS page cache) shared by sessions that are failing
# over or verifying at the same time. Each process has its own fixture, so a
# failure is attributable rather than a collision.
#
# Usage: scripts/race-soak-multi.sh [processes] [iterations] [attackers]
#
#   processes   concurrent soak processes (default 4)
#   iterations  reads each process performs (default 5000)
#   attackers   attacker threads inside each process (default 2)
#
# Exit status is non-zero if any process fails.

set -euo pipefail

cd "$(dirname "$0")/.."

processes="${1:-4}"
iterations="${2:-5000}"
attackers="${3:-2}"

echo "race soak: ${processes} process(es) x ${iterations} iterations x ${attackers} attacker(s)"

# Build once so the processes do not race each other through cargo's lock.
cargo test -p coderex-tools --lib --no-run --message-format=json \
    > target/race-soak-build.json
test_binary=$(python3 - <<'PY'
import json
path = None
for line in open("target/race-soak-build.json"):
    try:
        message = json.loads(line)
    except json.JSONDecodeError:
        continue
    if message.get("reason") == "compiler-artifact" and message.get("executable"):
        path = message["executable"]
if not path:
    raise SystemExit("the soak test binary was not produced")
print(path)
PY
)

mkdir -p target/race-soak
pids=()
for index in $(seq 1 "$processes"); do
    RACE_SOAK_ITERATIONS="$iterations" RACE_SOAK_ATTACKERS="$attackers" \
        "$test_binary" symlink_and_rename_churn --ignored --nocapture \
        > "target/race-soak/process-${index}.log" 2>&1 &
    pids+=("$!")
done

failed=0
for index in "${!pids[@]}"; do
    if ! wait "${pids[$index]}"; then
        failed=$((failed + 1))
        echo "process $((index + 1)) failed:" >&2
        tail -20 "target/race-soak/process-$((index + 1)).log" >&2
    fi
done

if [[ "$failed" -gt 0 ]]; then
    echo "${failed} of ${processes} soak process(es) failed" >&2
    exit 1
fi
echo "race soak: all ${processes} process(es) passed"
