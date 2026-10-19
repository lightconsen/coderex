#!/usr/bin/env bash
# Run one prompt against the real provider configured in the environment.
#
# Credentials are deliberately inherited: this is the same binary a person
# runs. The script refuses to start without ANTHROPIC_API_KEY so a missing
# secret fails loudly instead of looking like a model regression.
set -euo pipefail

PROMPT="${1:-hello}"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../../.." && pwd)
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
CODEREX_BIN="${CODEREX_BIN:-$TARGET_DIR/debug/coderex}"

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    echo "ANTHROPIC_API_KEY must be set for the live eval flow." >&2
    exit 1
fi

# The Read-tool case reads this file from the case's run directory, which is
# the working directory the harness gives this script.
printf 'live-tool-ok\n' > live-eval.txt

if [[ ! -x "$CODEREX_BIN" ]]; then
    cargo build --quiet \
        --manifest-path "$REPO_ROOT/Cargo.toml" \
        -p coderex-cli --bin coderex
fi

model_args=()
if [[ -n "${ANTHROPIC_MODEL:-}" ]]; then
    model_args=(--model "$ANTHROPIC_MODEL")
fi

# CODEREX_SKIP_ENV_SOURCE keeps a developer's local export_env.*.sh out of the
# run: the flow is driven by the environment the caller provided.
#
# Bash 3.2 (macOS) treats an empty `"${array[@]}"` under `set -u` as unbound,
# so the two shapes are spelled out instead of relying on a newer bash.
if [[ ${#model_args[@]} -gt 0 ]]; then
    CODEREX_SKIP_ENV_SOURCE=1 \
        "$CODEREX_BIN" --print --brief --dangerously-skip-permissions \
        "${model_args[@]}" "$PROMPT"
else
    CODEREX_SKIP_ENV_SOURCE=1 \
        "$CODEREX_BIN" --print --brief --dangerously-skip-permissions "$PROMPT"
fi
