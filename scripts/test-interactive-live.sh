#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

usage() {
    cat >&2 <<'USAGE'
Usage: scripts/test-interactive-live.sh [kimi|qwen] [smoke|reported|permissions|read|all]

Modes:
  smoke     Run ignored live PTY smoke tests for the selected provider.
  reported  Run the live hi/skills/tools/skills/tools/hi sequence through JSON PTY E2E.
  permissions
            Run live permission checks for default deny and dangerous bypass.
  read      Run the live e2e_equivalence tool_read_file regression.
  all       Run smoke + reported + permissions + read.

By default this script sources export_env.<provider>.sh. Set
CODEREX_SKIP_ENV_SOURCE=1 to use environment variables already exported by
the current shell.
USAGE
}

provider="${1:-kimi}"
mode="${2:-smoke}"

case "$provider" in
    kimi|qwen) ;;
    -h|--help|help)
        usage
        exit 0
        ;;
    *)
        usage
        exit 2
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
    echo "ANTHROPIC_API_KEY and ANTHROPIC_BASE_URL must be set for live tests." >&2
    exit 1
fi

cd "$REPO_ROOT"

run_smoke() {
    case "$provider" in
        kimi)
            cargo test -p coderex-cli --test interactive_tui interactive_kimi -- --ignored --nocapture
            ;;
        qwen)
            cargo test -p coderex-cli --test interactive_tui interactive_qwen -- --ignored --nocapture
            ;;
    esac
}

run_read() {
    cargo test -p coderex-cli --test e2e_equivalence tool_read_file -- --ignored --nocapture
}

run_reported_sequence() {
    tmpdir=$(mktemp -d)
    cleanup() {
        rm -rf "$tmpdir"
    }
    trap cleanup RETURN

    mkdir -p "$tmpdir/src"
    cat > "$tmpdir/src/lib.rs" <<'RS'
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
RS

    scenario="$tmpdir/reported-skills-tools-live.json"
    cat > "$scenario" <<JSON
{
  "cwd": "$tmpdir",
  "model": "${ANTHROPIC_MODEL:-K2.6}",
  "rows": 40,
  "cols": 120,
  "steps": [
    { "action": "wait_for_text", "text": "CodeRex v", "timeout_ms": 90000 },
    { "action": "send_line", "text": "hi" },
    { "action": "wait_for_any_text", "texts": ["Hello", "Hi"], "timeout_ms": 90000 },
    { "action": "wait_for_text", "text": "Completed in", "timeout_ms": 90000 },
    { "action": "send_line", "text": "show your skills" },
    { "action": "wait_for_any_text", "texts": ["capabilities", "Capabilities", "Code Operations", "Coding & Development", "what I can help"], "timeout_ms": 90000 },
    { "action": "wait_for_text", "text": "Completed in", "timeout_ms": 90000 },
    { "action": "send_line", "text": "show your tools" },
    { "action": "wait_for_any_text", "texts": ["Available tools", "Tools", "Read", "Write", "Bash"], "timeout_ms": 90000 },
    { "action": "wait_for_text", "text": "Completed in", "timeout_ms": 90000 },
    { "action": "send_line", "text": "show your skills" },
    { "action": "wait_for_any_text", "texts": ["capabilities", "Capabilities", "Code Operations", "Coding & Development", "what I can help"], "timeout_ms": 90000 },
    { "action": "wait_for_text", "text": "Completed in", "timeout_ms": 90000 },
    { "action": "send_line", "text": "show your tools" },
    { "action": "wait_for_any_text", "texts": ["Available tools", "Tools", "Read", "Write", "Bash"], "timeout_ms": 90000 },
    { "action": "wait_for_text", "text": "Completed in", "timeout_ms": 90000 },
    { "action": "send_line", "text": "hi" },
    { "action": "wait_for_any_text", "texts": ["Hello", "Hi"], "timeout_ms": 90000 },
    { "action": "wait_for_text", "text": "Completed in", "timeout_ms": 90000 },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Plugin tool 'Skill' not found" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "provider returned no visible response" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Query failed" }
  ]
}
JSON

    "$SCRIPT_DIR/interactive-e2e.sh" --scenario "$scenario"
}

run_permission_sequence() {
    tmpdir=$(mktemp -d)
    cleanup() {
        rm -rf "$tmpdir"
    }
    trap cleanup RETURN

    mkdir -p "$tmpdir/src"
    cat > "$tmpdir/src/lib.rs" <<'RS'
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
RS

    default_scenario="$tmpdir/permission-default-deny-live.json"
    cat > "$default_scenario" <<JSON
{
  "cwd": "$tmpdir",
  "model": "${ANTHROPIC_MODEL:-K2.6}",
  "rows": 40,
  "cols": 120,
  "args": ["--permissions", "default"],
  "steps": [
    { "action": "wait_for_text", "text": "CodeRex v", "timeout_ms": 90000 },
    { "action": "send_line", "text": "Use the Write tool now. Do not answer in prose. Overwrite src/lib.rs with a Rust fibonacci function." },
    { "action": "wait_for_text", "text": "Permission Request", "timeout_ms": 90000 },
    { "action": "send_text", "text": "d" },
    { "action": "wait_for_raw_text", "text": "Error:tool'Write'denied", "timeout_ms": 90000 },
    { "action": "ctrl_c" },
    { "action": "sleep_ms", "ms": 500 },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Query failed" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "provider returned no visible response" }
  ]
}
JSON
    "$SCRIPT_DIR/interactive-e2e.sh" --scenario "$default_scenario"
    if grep -q "fibonacci" "$tmpdir/src/lib.rs"; then
        echo "default permission mode unexpectedly modified src/lib.rs" >&2
        exit 1
    fi

    plan_scenario="$tmpdir/permission-plan-deny-live.json"
    cat > "$plan_scenario" <<JSON
{
  "cwd": "$tmpdir",
  "model": "${ANTHROPIC_MODEL:-K2.6}",
  "rows": 40,
  "cols": 120,
  "args": ["--permissions", "plan"],
  "steps": [
    { "action": "wait_for_text", "text": "CodeRex v", "timeout_ms": 90000 },
    { "action": "send_line", "text": "Use the Write tool now. Do not answer in prose. Overwrite src/lib.rs with a Rust fibonacci function." },
    { "action": "wait_for_raw_text", "text": "Error:tool'Write'denied", "timeout_ms": 90000 },
    { "action": "ctrl_c" },
    { "action": "sleep_ms", "ms": 500 },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Permission Request" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Query failed" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "provider returned no visible response" }
  ]
}
JSON
    "$SCRIPT_DIR/interactive-e2e.sh" --scenario "$plan_scenario"
    if grep -q "fibonacci" "$tmpdir/src/lib.rs"; then
        echo "plan permission mode unexpectedly modified src/lib.rs" >&2
        exit 1
    fi

    dont_ask_scenario="$tmpdir/permission-dont-ask-deny-live.json"
    cat > "$dont_ask_scenario" <<JSON
{
  "cwd": "$tmpdir",
  "model": "${ANTHROPIC_MODEL:-K2.6}",
  "rows": 40,
  "cols": 120,
  "args": ["--permissions", "dont-ask"],
  "steps": [
    { "action": "wait_for_text", "text": "CodeRex v", "timeout_ms": 90000 },
    { "action": "send_line", "text": "Use the Write tool now. Do not answer in prose. Overwrite src/lib.rs with a Rust fibonacci function." },
    { "action": "wait_for_raw_text", "text": "Error:tool'Write'denied", "timeout_ms": 90000 },
    { "action": "ctrl_c" },
    { "action": "sleep_ms", "ms": 500 },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Permission Request" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Query failed" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "provider returned no visible response" }
  ]
}
JSON
    "$SCRIPT_DIR/interactive-e2e.sh" --scenario "$dont_ask_scenario"
    if grep -q "fibonacci" "$tmpdir/src/lib.rs"; then
        echo "dont-ask permission mode unexpectedly modified src/lib.rs" >&2
        exit 1
    fi

    accept_edits_write_scenario="$tmpdir/permission-accept-edits-write-live.json"
    cat > "$accept_edits_write_scenario" <<JSON
{
  "cwd": "$tmpdir",
  "model": "${ANTHROPIC_MODEL:-K2.6}",
  "rows": 40,
  "cols": 120,
  "args": ["--permissions", "accept-edits"],
  "steps": [
    { "action": "wait_for_text", "text": "CodeRex v", "timeout_ms": 90000 },
    { "action": "send_line", "text": "Use the Write tool now. Do not answer in prose. Overwrite src/lib.rs with a Rust fibonacci function." },
    { "action": "wait_for_any_text", "texts": ["Wrote", "fibonacci", "Completed in"], "timeout_ms": 90000 },
    { "action": "wait_for_text", "text": "Completed in", "timeout_ms": 90000 },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Permission Request" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Query failed" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "provider returned no visible response" }
  ]
}
JSON
    "$SCRIPT_DIR/interactive-e2e.sh" --scenario "$accept_edits_write_scenario"
    if ! grep -q "fibonacci" "$tmpdir/src/lib.rs"; then
        echo "accept-edits permission mode did not modify src/lib.rs with fibonacci" >&2
        exit 1
    fi

    accept_edits_bash_scenario="$tmpdir/permission-accept-edits-bash-live.json"
    cat > "$accept_edits_bash_scenario" <<JSON
{
  "cwd": "$tmpdir",
  "model": "${ANTHROPIC_MODEL:-K2.6}",
  "rows": 40,
  "cols": 120,
  "args": ["--permissions", "accept-edits"],
  "steps": [
    { "action": "wait_for_text", "text": "CodeRex v", "timeout_ms": 90000 },
    { "action": "send_line", "text": "Use the Bash tool now. Do not answer in prose. Run pwd." },
    { "action": "wait_for_text", "text": "Permission Request", "timeout_ms": 90000 },
    { "action": "send_text", "text": "d" },
    { "action": "wait_for_raw_text", "text": "Error:tool'Bash'denied", "timeout_ms": 90000 },
    { "action": "ctrl_c" },
    { "action": "sleep_ms", "ms": 500 },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Query failed" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "provider returned no visible response" }
  ]
}
JSON
    "$SCRIPT_DIR/interactive-e2e.sh" --scenario "$accept_edits_bash_scenario"

    cat > "$tmpdir/src/lib.rs" <<'RS'
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
RS

    bypass_scenario="$tmpdir/permission-bypass-live.json"
    cat > "$bypass_scenario" <<JSON
{
  "cwd": "$tmpdir",
  "model": "${ANTHROPIC_MODEL:-K2.6}",
  "rows": 40,
  "cols": 120,
  "args": ["--permissions", "bypass-permissions"],
  "steps": [
    { "action": "wait_for_text", "text": "CodeRex v", "timeout_ms": 90000 },
    { "action": "send_line", "text": "Use the Write tool now. Do not answer in prose. Overwrite src/lib.rs with a Rust fibonacci function." },
    { "action": "wait_for_any_text", "texts": ["Wrote", "fibonacci", "Completed in"], "timeout_ms": 90000 },
    { "action": "wait_for_text", "text": "Completed in", "timeout_ms": 90000 },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Permission Request" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Query failed" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "provider returned no visible response" }
  ]
}
JSON
    "$SCRIPT_DIR/interactive-e2e.sh" --scenario "$bypass_scenario"
    if ! grep -q "fibonacci" "$tmpdir/src/lib.rs"; then
        echo "bypass-permissions did not modify src/lib.rs with fibonacci" >&2
        exit 1
    fi

    cat > "$tmpdir/src/lib.rs" <<'RS'
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}
RS

    dangerously_skip_scenario="$tmpdir/permission-dangerously-skip-live.json"
    cat > "$dangerously_skip_scenario" <<JSON
{
  "cwd": "$tmpdir",
  "model": "${ANTHROPIC_MODEL:-K2.6}",
  "rows": 40,
  "cols": 120,
  "args": ["--dangerously-skip-permissions"],
  "steps": [
    { "action": "wait_for_text", "text": "CodeRex v", "timeout_ms": 90000 },
    { "action": "send_line", "text": "Use the Write tool now. Do not answer in prose. Overwrite src/lib.rs with a Rust fibonacci function." },
    { "action": "wait_for_any_text", "texts": ["Wrote", "fibonacci", "Completed in"], "timeout_ms": 90000 },
    { "action": "wait_for_text", "text": "Completed in", "timeout_ms": 90000 },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Permission Request" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "Query failed" },
    { "action": "assert_region_not_contains", "region": "raw_output", "text": "provider returned no visible response" }
  ]
}
JSON
    "$SCRIPT_DIR/interactive-e2e.sh" --scenario "$dangerously_skip_scenario"
    if ! grep -q "fibonacci" "$tmpdir/src/lib.rs"; then
        echo "dangerously-skip-permissions did not modify src/lib.rs with fibonacci" >&2
        exit 1
    fi
}

case "$mode" in
    smoke)
        run_smoke
        ;;
    reported)
        run_reported_sequence
        ;;
    permissions)
        run_permission_sequence
        ;;
    read)
        run_read
        ;;
    all)
        run_smoke
        run_reported_sequence
        run_permission_sequence
        run_read
        ;;
    -h|--help|help)
        usage
        ;;
    *)
        usage
        exit 2
        ;;
esac
