#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

usage() {
    cat >&2 <<'USAGE'
Usage:
  scripts/test-code-agent-benchmark.sh mock [cargo-test-args...]
  scripts/test-code-agent-benchmark.sh live [kimi|qwen] [cargo-test-args...]

Runs hard-assert Code Agent benchmarks. Mock mode uses the deterministic provider
and is suitable for CI. Live mode sources export_env.<provider>.sh unless
CODEREX_SKIP_ENV_SOURCE=1, then runs ignored hard-gate live scenarios against
the selected provider. Set CODEREX_LIVE_BENCHMARK_TIMEOUT_SECS to override the
default live case timeout for slower providers.

A case only passes when the agent performs the expected file changes, the
fixture project passes cargo test, review-only tasks leave sources unchanged,
git/status/diff expectations hold, long-context and large-repo cases complete,
and forbidden failure/danger markers are absent. Each case emits BENCHMARK_CASE
line with duration_ms plus a BENCHMARK_SUMMARY pass-rate line.
USAGE
}

mode="${1:-mock}"
if [[ "$#" -gt 0 ]]; then
    shift
fi

cd "$REPO_ROOT"

run_and_report() {
    summary_mode="$1"
    shift

    tmp_output=$(mktemp)
    cleanup() {
        rm -f "$tmp_output"
    }
    trap cleanup RETURN

    set +e
    "$@" 2>&1 | tee "$tmp_output"
    status=${PIPESTATUS[0]}
    set -e

    cases=$(grep -c 'BENCHMARK_CASE' "$tmp_output" || true)
    duration_ms_total=$(awk -F'duration_ms=' '/BENCHMARK_CASE/ { sum += $2 } END { print sum + 0 }' "$tmp_output")
    passed=$(sed -nE 's/.*test result: ok\. ([0-9]+) passed.*/\1/p; s/.*test result: FAILED\. ([0-9]+) passed; ([0-9]+) failed.*/\1/p' "$tmp_output" | tail -n1)
    failed=$(sed -nE 's/.*test result: FAILED\. ([0-9]+) passed; ([0-9]+) failed.*/\2/p' "$tmp_output" | tail -n1)
    passed=${passed:-0}
    failed=${failed:-0}
    total=$((passed + failed))
    if [[ "$total" -gt 0 ]]; then
        pass_rate=$(awk -v p="$passed" -v t="$total" 'BEGIN { printf "%.2f", (p * 100) / t }')
    else
        pass_rate="0.00"
    fi

    echo "BENCHMARK_SUMMARY mode=$summary_mode passed=$passed failed=$failed total=$total pass_rate=${pass_rate}% cases_reported=$cases duration_ms_total=$duration_ms_total tool_calls=mock-observed tokens=not_available cost_usd=not_available"
    exit "$status"
}

run_mock() {
    if [[ "$#" -eq 0 ]]; then
        set -- --test-threads=1
    fi

    run_and_report "mock" cargo test -p coderex-cli --test code_agent_benchmark -- --nocapture "$@"
}

run_live() {
    provider="${1:-kimi}"
    case "$provider" in
        kimi|qwen)
            shift || true
            ;;
        *)
            provider="kimi"
            ;;
    esac

    if [[ "${CODEREX_SKIP_ENV_SOURCE:-0}" != "1" ]]; then
        env_file="$REPO_ROOT/export_env.$provider.sh"
        if [[ ! -f "$env_file" ]]; then
            echo "Missing provider env file: $env_file" >&2
            exit 1
        fi
        # shellcheck source=/dev/null
        source "$env_file"
    fi

    if [[ -z "${ANTHROPIC_API_KEY:-}" || -z "${ANTHROPIC_BASE_URL:-}" ]]; then
        echo "ANTHROPIC_API_KEY and ANTHROPIC_BASE_URL must be set for live benchmark tests." >&2
        exit 1
    fi

    if [[ "$#" -eq 0 ]]; then
        set -- --test-threads=1
    fi

    run_and_report "live:$provider" cargo test -p coderex-cli --test code_agent_benchmark live_hard_assert -- --ignored --nocapture "$@"
}

case "$mode" in
    mock)
        run_mock "$@"
        ;;
    live)
        run_live "$@"
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        usage
        exit 2
        ;;
esac
