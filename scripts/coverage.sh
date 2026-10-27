#!/usr/bin/env bash
# Generate workspace line coverage and a per-crate summary.
#
# Usage: scripts/coverage.sh [--summary-only] [output-dir]
#
# Writes lcov.info, coverage.tsv and summary.md under the output directory
# (default target/coverage). Test files under crates/*/tests, benches and
# examples are excluded from the denominator. The thresholds are advisory: this
# script always exits 0 after a successful test run so coverage never blocks
# local work; CI can turn the summary into a required check later.
#
# --summary-only regenerates the tables from an existing lcov.info.

set -euo pipefail

cd "$(dirname "$0")/.."
summary_only=false
if [[ "${1:-}" == "--summary-only" ]]; then
    summary_only=true
    shift
fi
out="${1:-target/coverage}"
mkdir -p "$out"

if [[ "$summary_only" == false ]]; then
    cargo llvm-cov --workspace --no-fail-fast --lcov --output-path "$out/lcov.info"
fi
if [[ ! -f "$out/lcov.info" ]]; then
    echo "missing $out/lcov.info; run without --summary-only first" >&2
    exit 1
fi

awk -F: '
    function crate_of(path, rest, parts) {
        if (path !~ /\/crates\/[^\/]+\//) return "other"
        rest = path
        sub(/^.*\/crates\//, "", rest)
        split(rest, parts, "/")
        return parts[1]
    }
    /^SF:/ {
        file = substr($0, 4)
        next
    }
    /^LF:/ {
        lf = $2
        next
    }
    /^LH:/ {
        lh = $2
        next
    }
    /^end_of_record/ {
        if (file != "" &&
            file !~ /\/tests\// &&
            file !~ /\/benches\// &&
            file !~ /\/examples\//) {
            crate = crate_of(file)
            total[crate] += lf
            hit[crate] += lh
        }
        file = ""
        next
    }
    END {
        for (crate in total) {
            printf "%s\t%d\t%d\n", crate, hit[crate], total[crate]
        }
    }
' "$out/lcov.info" | sort > "$out/coverage.tsv"

{
    printf '| crate | lines hit | lines | coverage |\n'
    printf '|---|---:|---:|---:|\n'
    awk -F '\t' '{
        if ($3 > 0) printf "| %s | %d | %d | %.1f%% |\n", $1, $2, $3, 100 * $2 / $3
    }' "$out/coverage.tsv"
} | tee "$out/summary.md"

echo
awk -F '\t' '{
    if ($3 == 0) next
    pct = 100 * $2 / $3
    target = ($1 == "core" || $1 == "cli" || $1 == "tools") ? 80 : 65
    if (pct < target) printf "below target: %-12s %.1f%% (target %d%%)\n", $1, pct, target
}' "$out/coverage.tsv"

echo
echo "lcov:    $out/lcov.info"
echo "tsv:     $out/coverage.tsv"
echo "summary: $out/summary.md"
echo "targets: core/cli/tools >= 80% lines; other crates >= 65% lines (advisory)"
