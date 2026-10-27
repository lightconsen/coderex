#!/usr/bin/env bash
# Record the LLM responses used by the provider-backed E2E suites.
#
# The suites run against `llm-cassette`; in record mode it proxies to the
# endpoint in your CodeRex settings and writes one JSON cassette per request
# under crates/cli/tests/cassettes/. Those recordings are committed, and every
# later run (local and CI) replays them without a provider, key or network.
#
# Usage:
#   scripts/record-cassettes.sh            # add missing recordings
#   scripts/record-cassettes.sh --fresh    # discard old recordings first
#   scripts/record-cassettes.sh --check    # metadata, age, size, secrets
#   scripts/record-cassettes.sh --backfill # add metadata to older recordings
#   scripts/record-cassettes.sh --prune    # replay and delete unused cassettes
#
# --check exits non-zero for malformed files, unmatched-request markers or
# credential-like strings. Staleness and size are warnings unless
# CASSETTE_STRICT=1. --prune replays every suite with tracing; a suite whose
# replay fails is left untouched.
#
# The endpoint and key are read from $CODEREX_SETTINGS, defaulting to
# ~/.coderex/settings.json and falling back to ~/.claude/settings.ds.json when
# the former has no provider configured. They are only exported to the test
# process; the script never prints them.

set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

case "${1:-}" in
  --check)
    shift
    exec python3 scripts/cassette_tool.py check "$@"
    ;;
  --backfill)
    shift
    exec python3 scripts/cassette_tool.py backfill "$@"
    ;;
  --prune)
    shift
    exec python3 scripts/cassette_tool.py prune "$@"
    ;;
esac

# Prefer an explicit file, then CodeRex's own settings, then the Claude Code
# settings that commonly hold a proxy provider (ANTHROPIC_* in `env`).
if [ -n "${CODEREX_SETTINGS:-}" ]; then
  settings="$CODEREX_SETTINGS"
else
  settings="$HOME/.coderex/settings.json"
  if ! grep -q "ANTHROPIC_BASE_URL\|base_url" "$settings" 2>/dev/null; then
    alt="$HOME/.claude/settings.ds.json"
    if [ -r "$alt" ] && grep -q "ANTHROPIC_BASE_URL" "$alt"; then
      settings="$alt"
    fi
  fi
fi
if [ ! -r "$settings" ]; then
  echo "error: cannot read $settings" >&2
  echo "point CODEREX_SETTINGS at the settings file that holds your provider" >&2
  exit 1
fi

endpoint=$(python3 - "$settings" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
env = data.get("env") or {}
provider = data.get("provider") or {}
base = provider.get("base_url") or env.get("ANTHROPIC_BASE_URL")
key = (
    data.get("api_key")
    or env.get("ANTHROPIC_API_KEY")
    or env.get("ANTHROPIC_AUTH_TOKEN")
)
if not base or not key:
    raise SystemExit(
        "settings have no Anthropic-compatible base_url/api_key (or env.ANTHROPIC_BASE_URL/AUTH_TOKEN)"
    )
print(base)
print(key)
PY
)
base_url=$(printf '%s\n' "$endpoint" | sed -n '1p')
api_key=$(printf '%s\n' "$endpoint" | sed -n '2p')
if [ -z "$base_url" ] || [ -z "$api_key" ]; then
  echo "error: could not read the endpoint from $settings" >&2
  exit 1
fi

export CODEREX_CASSETTE_MODE=record
export CODEREX_CASSETTE_UPSTREAM="$base_url"
export CODEREX_CASSETTE_API_KEY="$api_key"

if [ "${1:-}" = "--fresh" ]; then
  rm -rf crates/cli/tests/cassettes
fi
mkdir -p crates/cli/tests/cassettes

run() {
  echo "== recording $1"
  shift
  cargo test -p coderex-cli "$@" -- --test-threads=1
}

run live_basic_chat --test live_basic_chat
run live_bash_tool --test live_bash_tool
run live_file_read --test live_file_read
run e2e_code_agent --test e2e_code_agent
run e2e_equivalence --test e2e_equivalence
run code_agent_benchmark --test code_agent_benchmark
run interactive_tui_live --test interactive_tui interactive_live

echo
echo "recorded cassettes:"
find crates/cli/tests/cassettes -name '*.json' | wc -l | tr -d ' '
