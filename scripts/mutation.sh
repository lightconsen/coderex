#!/usr/bin/env bash
# Run cargo-mutants over a bounded scope.
#
# Usage: scripts/mutation.sh [scope] [extra cargo-mutants args...]
#
# Scopes:
#   permission-engine  crates/core/src/permission_engine.rs (default)
#   permissions        permissions.rs + permission_engine.rs
#   sandbox            core sandbox.rs + tools sandbox_shell.rs
#   hooks              crates/hooks/src/engine.rs
#   compact            crates/core/src/compact.rs
#   mcp                crates/mcp/src/channel.rs
#   all                every scope above
#
# The first run includes a baseline test pass. Results are written under
# target/mutants/ (mutants.out, caught.txt, missed.txt, ...). cargo-mutants
# copies the tree per mutant by default; set MUTANTS_IN_PLACE=1 to mutate the
# working tree in place (faster, but interrupts can leave a mutated file).
# MUTATION_TEST_FILTER=permission_engine::tests limits the test command to that
# filter; without it the whole mutated package's tests run (more thorough,
# slower, and some mutants time out rather than fail).
#
# This is intentionally non-blocking for local work: the script prints the
# missed-mutant list and exits with cargo-mutants' status.

set -euo pipefail

cd "$(dirname "$0")/.."

scope="${1:-permission-engine}"
if [[ $# -gt 0 ]]; then
    shift
fi

files=()
case "$scope" in
    permission-engine)
        files=(crates/core/src/permission_engine.rs)
        ;;
    permissions)
        files=(
            crates/core/src/permissions.rs
            crates/core/src/permission_engine.rs
        )
        ;;
    sandbox)
        files=(
            crates/core/src/sandbox.rs
            crates/tools/src/sandbox_shell.rs
        )
        ;;
    hooks)
        files=(crates/hooks/src/engine.rs)
        ;;
    compact)
        files=(crates/core/src/compact.rs)
        ;;
    mcp)
        files=(crates/mcp/src/channel.rs)
        ;;
    all)
        files=(
            crates/core/src/permission_engine.rs
            crates/core/src/permissions.rs
            crates/core/src/sandbox.rs
            crates/core/src/compact.rs
            crates/tools/src/sandbox_shell.rs
            crates/hooks/src/engine.rs
            crates/mcp/src/channel.rs
        )
        ;;
    -h|--help|help)
        sed -n '2,25p' "$0"
        exit 0
        ;;
    *)
        echo "unknown scope: $scope" >&2
        echo "scopes: permission-engine permissions sandbox hooks compact mcp all" >&2
        exit 2
        ;;
esac

command -v cargo-mutants >/dev/null 2>&1 || {
    echo "cargo-mutants is not installed; run: cargo install cargo-mutants --locked" >&2
    exit 1
}

args=(--no-times --timeout 60 --output target/mutants)
if [[ "${MUTANTS_IN_PLACE:-0}" != "0" ]]; then
    args+=(--in-place)
fi
for file in "${files[@]}"; do
    args+=(--file "$file")
done
if [[ -n "${MUTATION_TEST_FILTER:-}" ]]; then
    args+=(-- "$MUTATION_TEST_FILTER")
fi

echo "mutation scope: $scope"
printf '  %s\n' "${files[@]}"
echo "output: target/mutants"
echo

status=0
cargo mutants "${args[@]}" "$@" || status=$?

if [[ -d target/mutants/mutants.out ]]; then
    echo
    for report in missed.txt caught.txt timeout.txt unviable.txt; do
        path="target/mutants/mutants.out/$report"
        [[ -f "$path" ]] && echo "$report: $(wc -l < "$path" | tr -d ' ')"
    done
fi
exit "$status"
