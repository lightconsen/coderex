#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

usage() {
    cat >&2 <<'USAGE'
Usage: scripts/test-interactive-mock.sh [focused|full|matrix|permissions|reported|e2e]

Modes:
  focused   Run the high-signal mock interactive regressions plus cargo check.
  full      Run the full mock interactive TUI suite plus E2E tool smoke and cargo check.
  matrix    Run only the fixed sequence-matrix PTY tests.
  permissions
            Run only the mock interactive permission-mode matrix.
  reported  Run only the reported skills/tools sequence regression.
  e2e       Run only the JSON-driven mock interactive E2E smoke.
USAGE
}

run_matrix() {
    cargo test -p coderex-cli --test interactive_tui sequence_matrix -- --nocapture
}

run_reported() {
    cargo test -p coderex-cli --test interactive_tui reported_skills_tools_sequence_stays_usable -- --nocapture
}

run_permissions() {
    cargo test -p coderex-cli --test interactive_tui permission_mode_sequence_matrix_enforces_expected_policy -- --nocapture
}

run_e2e() {
    cargo test -p coderex-cli --test interactive_e2e_tool -- --nocapture
}

cd "$REPO_ROOT"

mode="${1:-focused}"
case "$mode" in
    focused)
        run_matrix
        run_reported
        run_e2e
        cargo check -p coderex-cli
        ;;
    full)
        cargo test -p coderex-cli --test interactive_tui -- --nocapture
        run_e2e
        cargo check -p coderex-cli
        ;;
    matrix)
        run_matrix
        ;;
    permissions)
        run_permissions
        ;;
    reported)
        run_reported
        ;;
    e2e)
        run_e2e
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        usage
        exit 2
        ;;
esac
