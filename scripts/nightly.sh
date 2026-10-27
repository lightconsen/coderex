#!/usr/bin/env bash
# Nightly verification, callable from cron, a self-hosted Linux runner, or CI.
#
# Usage: scripts/nightly.sh
#
# Environment:
#   RUN_COVERAGE=1      run scripts/coverage.sh
#   RUN_CASSETTES=1     run scripts/record-cassettes.sh --check
#   RUN_FUZZ=1          run scripts/fuzz.sh
#   RUN_SOAK=1          run a 200-turn soak
#   RUN_MUTATION=0      run scripts/mutation.sh (expensive; off by default)
#   FUZZ_SECONDS=600    seconds per fuzz target
#   SOAK_TURNS=200
#   MUTATION_SCOPE=permission-engine
#   MUTATION_STRICT=0   fail the run when cargo-mutants reports missed mutants
#   NIGHTLY_OUTPUT=target/nightly/<UTC timestamp>
#
# Every step writes a log under the output directory. The script exits non-zero
# when a step that is expected to succeed fails; mutation findings are reported
# but do not fail the run unless MUTATION_STRICT=1.

set -uo pipefail

cd "$(dirname "$0")/.."

run_coverage="${RUN_COVERAGE:-1}"
run_cassettes="${RUN_CASSETTES:-1}"
run_fuzz="${RUN_FUZZ:-1}"
run_soak="${RUN_SOAK:-1}"
run_mutation="${RUN_MUTATION:-0}"
fuzz_seconds="${FUZZ_SECONDS:-600}"
soak_turns="${SOAK_TURNS:-200}"
mutation_scope="${MUTATION_SCOPE:-permission-engine}"
mutation_strict="${MUTATION_STRICT:-0}"
out="${NIGHTLY_OUTPUT:-target/nightly/$(date -u +%Y%m%dT%H%M%SZ)}"

mkdir -p "$out"
status=0
declare -a results=()

step() {
    local name="$1"
    shift
    echo "== $name"
    local log="$out/$name.log"
    if "$@" > "$log" 2>&1; then
        echo "   ok ($log)"
        results+=("ok   $name")
    else
        local code=$?
        echo "   failed:$code ($log)" >&2
        results+=("fail $name (exit $code)")
        status=1
    fi
}

if [[ "$run_coverage" != "0" ]]; then
    step coverage ./scripts/coverage.sh "$out/coverage"
fi
if [[ "$run_cassettes" != "0" ]]; then
    step cassettes ./scripts/record-cassettes.sh --check
fi
if [[ "$run_fuzz" != "0" ]]; then
    step fuzz ./scripts/fuzz.sh "$fuzz_seconds"
fi
if [[ "$run_soak" != "0" ]]; then
    step soak env SOAK_TURNS="$soak_turns" SOAK_OUTPUT_DIR="$out/soak" python3 scripts/soak.py
fi
if [[ "$run_mutation" != "0" ]]; then
    echo "== mutation ($mutation_scope)"
    log="$out/mutation.log"
    if ./scripts/mutation.sh "$mutation_scope" > "$log" 2>&1; then
        echo "   ok ($log)"
        results+=("ok   mutation")
    else
        code=$?
        # cargo-mutants exits 2 when it found missed mutants: that is a
        # finding, not an infrastructure failure.
        if [[ "$code" == "2" && "$mutation_strict" == "0" ]]; then
            echo "   findings only, exit 2 ($log)"
            results+=("warn mutation (missed mutants)")
        else
            echo "   failed:$code ($log)" >&2
            results+=("fail mutation (exit $code)")
            status=1
        fi
    fi
fi

{
    echo "# Nightly $out"
    echo
    if [[ ${#results[@]} -gt 0 ]]; then
        printf -- '- %s\n' "${results[@]}"
    fi
    echo
    for report in coverage/summary.md; do
        if [[ -f "$out/$report" ]]; then
            echo "## Coverage"
            echo
            cat "$out/$report"
            echo
        fi
    done
    if [[ -f "$out/soak/summary.json" ]]; then
        echo "## Soak"
        echo
        echo '```json'
        cat "$out/soak/summary.json"
        echo '```'
    fi
} > "$out/summary.md"

echo
echo "nightly summary: $out/summary.md"
cat "$out/summary.md"
exit "$status"
