#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

if [ "$#" -eq 0 ]; then
    echo "Usage: $0 --scenario /path/to/scenario.json" >&2
    exit 1
fi

cd "$REPO_ROOT"
cargo build -p coderex-cli --bin coderex --bin coderex-interactive-e2e
exec "$REPO_ROOT/target/debug/coderex-interactive-e2e" "$@"
