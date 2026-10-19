#!/usr/bin/env bash
# Run the checked-in live-provider eval flow.
#
# Inputs (environment):
#   ANTHROPIC_API_KEY   required; the script refuses to run without it
#   ANTHROPIC_BASE_URL  optional; an Anthropic-compatible endpoint
#   ANTHROPIC_MODEL     optional; passed through to the CLI as --model
#   CODEREX_BIN         optional; an already-built coderex binary
#
# Outputs:
#   evals/flows/live/baseline/results.jsonl
#   evals/flows/live/report.html
#   evals/flows/live/_state.json (approved harness hash)
#
# Exits non-zero when any case fails, so it can gate a workflow job.
set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

usage() {
    cat >&2 <<'USAGE'
Usage: scripts/test-live-eval.sh [run-eval options]

Runs evals/flows/live against the real provider configured by
ANTHROPIC_API_KEY (and optionally ANTHROPIC_BASE_URL / ANTHROPIC_MODEL).

Examples:
  ANTHROPIC_API_KEY=... scripts/test-live-eval.sh
  ANTHROPIC_API_KEY=... scripts/test-live-eval.sh --case live-text-turn

Extra options are passed through to run-eval (--case, --reps, --timeout-s, ...).
USAGE
}

case "${1:-}" in
    -h|--help|help)
        usage
        exit 0
        ;;
esac

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    echo "ANTHROPIC_API_KEY is not set; refusing to run the live provider eval." >&2
    usage
    exit 1
fi

if [[ -z "${ANTHROPIC_BASE_URL:-}" ]]; then
    # A workflow secret that is not configured arrives as an empty string;
    # unset it so the CLI applies its own default instead of parsing "".
    unset ANTHROPIC_BASE_URL
    echo "note: ANTHROPIC_BASE_URL is not set; using the CLI's default endpoint." >&2
fi

cd "$REPO_ROOT"

cargo build -p coderex-cli --bin coderex
cargo run -p coderex-evals --bin run-eval -- \
    --flow-dir "$REPO_ROOT/evals/flows/live" \
    --variant baseline \
    --approve-harness \
    --force \
    "$@"
