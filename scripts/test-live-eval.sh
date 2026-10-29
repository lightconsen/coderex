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

# The provider may come from the environment or from the settings file the
# rest of the tooling reads. `CODEREX_LIVE_EVAL_FROM_SETTINGS=1` forces the
# settings: useful when the shell already exports another harness's
# ANTHROPIC_* variables, since a base URL and a key from different providers
# cannot be mixed.
from_settings="${CODEREX_LIVE_EVAL_FROM_SETTINGS:-0}"
if [[ "$from_settings" != "0" || -z "${ANTHROPIC_API_KEY:-}" ]]; then
    provider_lines=""
    used_settings=""
    for candidate in "${CODEREX_SETTINGS:-}" "$HOME/.coderex/settings.json" "$HOME/.claude/settings.ds.json"; do
        [[ -n "$candidate" && -r "$candidate" ]] || continue
        provider_lines=$(python3 - "$candidate" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
env = data.get("env") or {}
provider = data.get("provider") or {}
base = provider.get("base_url") or env.get("ANTHROPIC_BASE_URL") or ""
key = (
    data.get("api_key")
    or env.get("ANTHROPIC_API_KEY")
    or env.get("ANTHROPIC_AUTH_TOKEN")
    or ""
)
model = (
    data.get("model")
    or env.get("ANTHROPIC_MODEL")
    or env.get("ANTHROPIC_DEFAULT_SONNET_MODEL")
    or ""
)
if not key:
    raise SystemExit(1)
print(base)
print(key)
print(model)
PY
        ) || true
        if [[ -n "$provider_lines" ]]; then
            used_settings="$candidate"
            break
        fi
    done
    if [[ -n "$provider_lines" ]]; then
        settings_base=$(printf '%s\n' "$provider_lines" | sed -n '1p')
        settings_key=$(printf '%s\n' "$provider_lines" | sed -n '2p')
        settings_model=$(printf '%s\n' "$provider_lines" | sed -n '3p')
        ANTHROPIC_API_KEY="$settings_key"
        export ANTHROPIC_API_KEY
        if [[ -n "$settings_base" ]] && { [[ "$from_settings" != "0" ]] || [[ -z "${ANTHROPIC_BASE_URL:-}" ]]; }; then
            ANTHROPIC_BASE_URL="$settings_base"
            export ANTHROPIC_BASE_URL
        fi
        if [[ -n "$settings_model" ]] && { [[ "$from_settings" != "0" ]] || [[ -z "${ANTHROPIC_MODEL:-}" ]]; }; then
            ANTHROPIC_MODEL="$settings_model"
            export ANTHROPIC_MODEL
        fi
        echo "note: using the provider from $used_settings (the key is not printed)." >&2
    fi
fi

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
# Approve the harness only on the first run. After that the pinned hash is
# enforced, so a changed flow is refused instead of silently re-approved.
approve=()
if [ ! -f "$REPO_ROOT/evals/flows/live/_state.json" ]; then
    approve=(--approve-harness)
fi
cargo run -p coderex-evals --bin run-eval -- \
    --flow-dir "$REPO_ROOT/evals/flows/live" \
    --variant baseline \
    ${approve[@]+"${approve[@]}"} \
    --force \
    "$@"
