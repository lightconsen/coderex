#!/usr/bin/env bash
# Verification checks that are not part of the normal test gate:
# dependency advisories, license policy, rustdoc warnings and clippy.
#
# Usage: scripts/verify.sh [check ...]
#
# Checks:
#   audit     cargo audit with .cargo/audit.toml (reviewed ignores)
#   deny      cargo deny check (advisories, bans, licenses, sources)
#   clippy    cargo clippy --workspace --all-targets, warning count
#   rustdoc   cargo doc --workspace --no-deps, warning count
#   all       every check above (default)
#
# Each check prints its current count and the actionable findings. The script
# exits non-zero when a check fails; `clippy` and `rustdoc` report their warning
# counts and only fail when --strict is set (or STRICT=1), because the backlog
# is still being paid down. Audit and deny are always strict.
#
# Output: target/verify/<check>.log, and target/verify/summary.md.

set -uo pipefail

cd "$(dirname "$0")/.."

strict="${STRICT:-0}"
if [[ "${1:-}" == "--strict" ]]; then
    strict=1
    shift
fi

if [[ $# -gt 0 ]]; then
    checks=("$@")
else
    checks=(audit deny clippy rustdoc)
fi

out="target/verify"
mkdir -p "$out"
status=0
declare -a results=()

# Expand `all` before iterating: a `for` loop captures its list once.
expanded=()
for check in "${checks[@]}"; do
    if [[ "$check" == "all" ]]; then
        expanded+=(audit deny clippy rustdoc)
    else
        expanded+=("$check")
    fi
done
checks=("${expanded[@]}")

need() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "$1 is not installed; run: $2" >&2
        results+=("skip $1 (not installed)")
        return 1
    }
}

run_audit() {
    local code
    if [[ "${AUDIT_OFFLINE:-0}" != "0" ]]; then
        cargo audit --quiet --no-fetch --stale > "$out/audit.log" 2>&1
        code=$?
    else
        cargo audit --quiet > "$out/audit.log" 2>&1
        code=$?
        # No network is not a finding: reuse the local advisory database and
        # say that the result may be stale.
        if [[ $code -ne 0 ]] && grep -q "couldn't fetch advisory database" "$out/audit.log"; then
            echo "   advisory fetch failed; retrying with the local database"
            cargo audit --quiet --no-fetch --stale > "$out/audit.log" 2>&1
            code=$?
            [[ $code -eq 0 ]] && echo "   (stale database: run with network to refresh)"
        fi
    fi
    if [[ $code -eq 0 ]]; then
        echo "   ok — no unignored advisories"
        results+=("ok   audit")
    else
        echo "   failed:$code — see $out/audit.log"
        grep -E "^(error|Crate:|ID:)" "$out/audit.log" | head -20
        results+=("fail audit (exit $code)")
        status=1
    fi
}

run_deny() {
    local code
    if [[ "${DENY_OFFLINE:-0}" != "0" ]]; then
        cargo deny --offline check > "$out/deny.log" 2>&1
        code=$?
    else
        cargo deny check > "$out/deny.log" 2>&1
        code=$?
        if [[ $code -ne 0 ]] && grep -q "failed to fetch advisory database" "$out/deny.log"; then
            echo "   advisory fetch failed; retrying offline"
            cargo deny --offline check > "$out/deny.log" 2>&1
            code=$?
            [[ $code -eq 0 ]] && echo "   (offline: advisory database may be stale)"
        fi
    fi
    if [[ $code -eq 0 ]]; then
        echo "   ok — advisories, bans, licenses, sources"
        results+=("ok   deny")
    else
        echo "   failed:$code — see $out/deny.log"
        grep -E "^(error|advisories|bans|licenses|sources)" "$out/deny.log" | tail -10
        results+=("fail deny (exit $code)")
        status=1
    fi
}

run_clippy() {
    cargo clippy --workspace --all-targets > "$out/clippy.log" 2>&1
    local code=$?
    local warnings
    warnings=$(grep -c "^warning: " "$out/clippy.log")
    echo "   $warnings warning(s), exit $code"
    if [[ $code -ne 0 ]]; then
        grep -E "^error" "$out/clippy.log" | head -10
        results+=("fail clippy (exit $code)")
        status=1
    elif [[ "$warnings" -gt 0 && "$strict" == "1" ]]; then
        results+=("fail clippy ($warnings warnings, strict)")
        status=1
    else
        results+=("warn clippy ($warnings warnings)")
    fi
}

run_rustdoc() {
    RUSTDOCFLAGS="" cargo doc --workspace --no-deps > "$out/rustdoc.log" 2>&1
    local code=$?
    local warnings
    warnings=$(grep -c "^warning" "$out/rustdoc.log")
    echo "   $warnings warning(s), exit $code"
    if [[ $code -ne 0 ]]; then
        grep -E "^error" "$out/rustdoc.log" | head -10
        results+=("fail rustdoc (exit $code)")
        status=1
    elif [[ "$warnings" -gt 0 && "$strict" == "1" ]]; then
        results+=("fail rustdoc ($warnings warnings, strict)")
        status=1
    else
        results+=("warn rustdoc ($warnings warnings)")
    fi
}

for check in "${checks[@]}"; do
    case "$check" in
        audit)
            need cargo-audit "cargo install cargo-audit --locked" || continue
            echo "== audit"
            run_audit
            ;;
        deny)
            need cargo-deny "cargo install cargo-deny --locked" || continue
            echo "== deny"
            run_deny
            ;;
        clippy)
            echo "== clippy"
            run_clippy
            ;;
        rustdoc|doc)
            echo "== rustdoc"
            run_rustdoc
            ;;
        all)
            # Expanded before the loop starts; reaching this means it was
            # already handled.
            ;;
        *)
            echo "unknown check: $check (audit deny clippy rustdoc all)" >&2
            exit 2
            ;;
    esac
done

{
    echo "# Verification $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo
    printf -- '- %s\n' "${results[@]}"
    echo
    echo "Warning counts (clippy / rustdoc) are informational until the backlog"
    echo "is cleared; \`--strict\` turns them into failures."
} | tee "$out/summary.md"

exit "$status"
