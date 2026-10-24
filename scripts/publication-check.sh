#!/usr/bin/env bash
# Pre-publication audit.
#
# Answers one question: if this repository (working tree and history) were
# published today, would it contain a credential or a local working file?
#
# Checks:
#   1. tracked files for secret-shaped strings
#   2. every commit on every ref (git log -G)
#   3. tracked files that must stay local (*.local.md, env files, settings)
#   4. untracked files that look sensitive and would be staged by `git add -A`
#   5. gitleaks over the history, when it is installed
#
# Secret values are never printed: only the location and a short prefix.
#
# Usage: scripts/publication-check.sh
# Exit status: 0 clean, 1 findings, 2 not a git repository.

set -u
set -o pipefail

root=$(git rev-parse --show-toplevel 2>/dev/null) || {
  echo "not inside a git repository" >&2
  exit 2
}
cd "$root" || exit 2

findings=0
note() { printf '%s\n' "$*"; }
finding() {
  findings=$((findings + 1))
  printf '  FINDING  %s\n' "$*"
}

# Tokens that are deliberately fake: the telemetry scrubber's test fixtures and
# the mock provider examples.
is_fixture() {
  case "$1" in
    *sk-abc* | *sk-live-abc* | *ABCDEFGHIJKLMNOPQRSTUVWXYZ* | \
      *AKIAIOSFODNN7EXAMPLE* | *xoxb-1234567890* | *secretbody* | *MIIE* | \
      *11ABCDEFG* | *sk-abcdefghijklmnopqrstuvwxyz* | *A1b2C3d4* | \
      *dGhpcy1pcy1h* | *0123456789abcdef0123456789abcdef01234567* | \
      *AbcAbcAbc*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

mask() { printf '%s' "$1" | cut -c1-10; }

# Provider keys, VCS tokens, cloud keys, private-key headers, and an
# Anthropic-style auth token assignment with a real-looking value.
PATTERN='(sk-[A-Za-z0-9_-]{16,}|sk-ant-[A-Za-z0-9_-]{10,}|ghp_[A-Za-z0-9]{30,}|gho_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{20,}|AKIA[0-9A-Z]{16}|xox[baprs]-[A-Za-z0-9-]{10,}|AIza[0-9A-Za-z_-]{30,}|-----BEGIN [A-Z ]*PRIVATE KEY-----|ANTHROPIC_AUTH_TOKEN[[:space:]]*[:=][[:space:]]*.?[A-Za-z0-9_-]{20,})'

note "== 1. tracked files =="
while IFS= read -r hit; do
  file=${hit%%:*}
  rest=${hit#*:}
  line=${rest%%:*}
  token=${rest#*:}
  is_fixture "$token" && continue
  # A fixture marker usually sits on the same source line as the matched
  # string (e.g. the PEM test above), even when it is outside the match.
  full=$(sed -n "${line}p" "$file" 2>/dev/null)
  is_fixture "$full" && continue
  finding "tracked ${file}:${line}: $(mask "$token")..."
done < <(git grep -n -I -oE "$PATTERN" -- . 2>/dev/null)

note "== 2. git history (all refs; additions only) =="
commit=""
last_commit=""
last_token=""
while IFS= read -r line; do
  case "$line" in
    @@@*)
      commit=${line#@@@}
      continue
      ;;
  esac
  token=$(printf '%s\n' "$line" | grep -oE "$PATTERN" | head -n 1)
  [ -n "$token" ] || continue
  is_fixture "$token" && continue
  is_fixture "$line" && continue
  if [ "$token" = "$last_token" ] && [ "$commit" = "$last_commit" ]; then
    continue
  fi
  last_token=$token
  last_commit=$commit
  finding "history ${commit%% *}: $(mask "$token")... (${commit#* })"
done < <(git log --all -p -G "$PATTERN" --pretty=format:'@@@%H %s' 2>/dev/null)

note "== 3. tracked files that must stay local =="
while IFS= read -r file; do
  [ -n "$file" ] || continue
  case "$file" in
    export_env.local.sh)
      # Deliberately tracked: it only carries the mock server settings.
      if grep -qE '^export[[:space:]]+ANTHROPIC_API_KEY=' "$file" &&
        ! grep -qE '^export[[:space:]]+ANTHROPIC_API_KEY="?mock"?$' "$file"; then
        finding "${file} is tracked but no longer holds only the mock key"
      fi
      ;;
    *)
      finding "tracked file that should stay local: $file"
      ;;
  esac
done < <(git ls-files -- '*.local.md' '*.env' '.env*' '*settings*.json' '*.pem' '*.key' '*.p12' '*.sqlite' '*.db' 'export_env.*.sh')

if ! grep -qxF '*.local.md' .gitignore; then
  finding ".gitignore does not ignore *.local.md"
fi

note "== 4. untracked files that look sensitive =="
while IFS= read -r file; do
  case "$file" in
    *.local.md | *.env | .env* | *settings*.json | *.pem | *.key | *.p12 | *.sqlite | *.db)
      finding "untracked and sensitive-looking (git add -A would publish it): $file"
      ;;
  esac
done < <(git status --porcelain --untracked-files=all 2>/dev/null | sed -n 's/^?? //p')

note "== 5. gitleaks =="
if command -v gitleaks >/dev/null 2>&1; then
  if ! gitleaks detect --source . --log-opts="--all" --redact --no-banner -v; then
    finding "gitleaks reported findings"
  fi
else
  note "  gitleaks is not installed (optional); the built-in scan above stands in"
fi

if [ "$findings" -eq 0 ]; then
  note "publication-check: clean"
  exit 0
fi
note "publication-check: $findings finding(s)"
exit 1
