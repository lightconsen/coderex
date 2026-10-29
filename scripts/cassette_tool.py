#!/usr/bin/env python3
"""Maintain the committed LLM cassettes.

Subcommands:
  check [--max-age-days N] [--max-mb N] [--strict]
  backfill [--no-git-date]
  prune [--dry-run]

`check` never modifies files. It reports malformed/missing-marker files,
recordings without metadata, stale recordings and suites over the size budget.
`backfill` fills metadata for recordings made before metadata existed.
`prune` replays each suite with tracing and deletes cassettes the suite no
longer requests.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CASSETTES = ROOT / "crates/cli/tests/cassettes"

# suite -> [(test binary, optional name filter)]
SUITES: dict[str, list[tuple[str, str | None]]] = {
    "live": [
        ("live_basic_chat", None),
        ("live_bash_tool", None),
        ("live_file_read", None),
    ],
    "e2e_code_agent": [("e2e_code_agent", None)],
    "e2e_equivalence": [("e2e_equivalence", None)],
    "benchmark_live": [("code_agent_benchmark", None)],
    "interactive_tui_live": [("interactive_tui", "interactive_live")],
}

# The digest the recorder writes into metadata: lowercase SHA-256 hex.
DIGEST_PATTERN = re.compile(r"[0-9a-f]{64}")

SECRET_PATTERNS = [
    re.compile(r"sk-ant-[A-Za-z0-9_\-]{10,}"),
    re.compile(r"sk-[A-Za-z0-9]{20,}"),
    re.compile(r"(?i)authorization\s*[:=]\s*bearer\s+[A-Za-z0-9._\-]{20,}"),
    re.compile(r"(?i)x-api-key\s*[:=]\s*[A-Za-z0-9._\-]{20,}"),
]


def cassette_files() -> list[Path]:
    if not CASSETTES.is_dir():
        return []
    return sorted(p for p in CASSETTES.rglob("*.json") if p.is_file())


def parse_time(value: object) -> dt.datetime | None:
    if not isinstance(value, str) or not value.strip():
        return None
    try:
        return dt.datetime.fromisoformat(value.strip().replace("Z", "+00:00"))
    except ValueError:
        return None


def human_size(size: int) -> str:
    return f"{size / (1024 * 1024):.1f} MiB"


def metadata_of(document: dict) -> dict:
    metadata = document.get("metadata")
    return metadata if isinstance(metadata, dict) else {}


def check(args: argparse.Namespace) -> int:
    max_age = args.max_age_days
    max_bytes = args.max_mb * 1024 * 1024
    now = dt.datetime.now(dt.timezone.utc)
    files = cassette_files()
    errors: list[str] = []
    warnings: list[str] = []
    missing_metadata: list[Path] = []
    stale: list[tuple[Path, int]] = []
    missing_endpoint: list[Path] = []
    missing_test: list[Path] = []
    missing_digest: list[Path] = []
    bad_digest: list[Path] = []
    backfilled = 0
    malformed: list[Path] = []
    markers: list[Path] = []
    secrets: list[Path] = []
    suite_sizes: dict[str, int] = {}
    total_size = 0

    for path in files:
        suite = path.parent.name
        size = path.stat().st_size
        total_size += size
        suite_sizes[suite] = suite_sizes.get(suite, 0) + size
        text = path.read_text(encoding="utf-8", errors="replace")
        for pattern in SECRET_PATTERNS:
            if pattern.search(text):
                secrets.append(path)
                break
        if path.name.startswith("_missing_"):
            markers.append(path)
            continue
        try:
            document = json.loads(text)
        except json.JSONDecodeError as error:
            malformed.append(path)
            errors.append(f"malformed JSON: {path.relative_to(ROOT)}: {error}")
            continue
        metadata = metadata_of(document)
        recorded_at = parse_time(metadata.get("recorded_at"))
        if recorded_at is None:
            missing_metadata.append(path)
            continue
        age = (now - recorded_at).days
        if age > max_age:
            stale.append((path, age))
        if metadata.get("backfilled"):
            backfilled += 1
            continue
        if not metadata.get("endpoint_host"):
            missing_endpoint.append(path)
        if not metadata.get("test"):
            missing_test.append(path)
        digest = metadata.get("system_sha256")
        if digest is None:
            missing_digest.append(path)
        elif not DIGEST_PATTERN.fullmatch(str(digest)):
            bad_digest.append(path)

    print(f"cassettes: {len(files)} files, {human_size(total_size)}, {len(suite_sizes)} suites")
    if backfilled:
        print(f"  backfilled metadata: {backfilled} (endpoint/test were not recorded)")
    for suite in sorted(suite_sizes):
        print(f"  {suite:<22} {human_size(suite_sizes[suite])}")
        if suite_sizes[suite] > max_bytes:
            warnings.append(
                f"suite {suite} is {human_size(suite_sizes[suite])} (budget {args.max_mb} MiB)"
            )
    if total_size > max_bytes:
        warnings.append(
            f"cassette tree is {human_size(total_size)} (budget {args.max_mb} MiB)"
        )
    if missing_metadata:
        warnings.append(
            f"{len(missing_metadata)} recordings have no recorded_at; run --backfill"
        )
    if markers:
        warnings.append(
            f"{len(markers)} unmatched-request markers; run --prune to remove them"
        )
    if missing_endpoint:
        warnings.append(
            f"{len(missing_endpoint)} recordings have no endpoint_host (backfilled or older)"
        )
    if missing_test:
        warnings.append(f"{len(missing_test)} recordings have no test name")
    if stale:
        warnings.append(
            f"{len(stale)} recordings are older than {max_age} days; oldest "
            f"{max(age for _, age in stale)} days"
        )
    if missing_digest:
        warnings.append(
            f"{len(missing_digest)} recordings predate the system-prompt digest; "
            "replay cannot detect prompt drift for them until they are re-recorded"
        )
    if bad_digest:
        errors.append(
            f"{len(bad_digest)} recordings have a malformed system_sha256"
        )
    if malformed:
        errors.append(f"{len(malformed)} malformed cassette files")
    if secrets:
        errors.append(f"{len(secrets)} files contain a credential-like string")
        for path in secrets[:10]:
            print(f"  secret: {path.relative_to(ROOT)}", file=sys.stderr)

    for message in warnings:
        print(f"warning: {message}", file=sys.stderr)
    for message in errors:
        print(f"error: {message}", file=sys.stderr)

    strict = args.strict or os.environ.get("CASSETTE_STRICT", "") not in ("", "0", "false", "no")
    if errors or (strict and warnings):
        return 1
    return 0


def git_date(path: Path) -> str | None:
    try:
        result = subprocess.run(
            ["git", "log", "-1", "--format=%cs", "--", str(path.relative_to(ROOT))],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return None
    value = result.stdout.strip()
    return value or None


def backfill(args: argparse.Namespace) -> int:
    files = cassette_files()
    changed = 0
    for path in files:
        if path.name.startswith("_missing_"):
            continue
        try:
            document = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        metadata = document.get("metadata")
        if not isinstance(metadata, dict):
            metadata = {}
            document["metadata"] = metadata
        before = dict(metadata)
        if not metadata.get("recorded_at"):
            date = None if args.no_git_date else git_date(path)
            if date:
                metadata["recorded_at"] = f"{date}T00:00:00Z"
            else:
                stamp = dt.datetime.fromtimestamp(
                    path.stat().st_mtime, tz=dt.timezone.utc
                )
                metadata["recorded_at"] = stamp.replace(microsecond=0).isoformat().replace(
                    "+00:00", "Z"
                )
        if not metadata.get("suite"):
            metadata["suite"] = path.parent.name
        if not metadata.get("model") or metadata.get("model") == "<model>":
            model = document.get("request", {}).get("model")
            if isinstance(model, str) and model and model != "<model>":
                metadata["model"] = model
            else:
                metadata.pop("model", None)
        metadata["backfilled"] = True
        if metadata != before:
            path.write_text(
                json.dumps(document, indent=2, ensure_ascii=False) + "\n",
                encoding="utf-8",
            )
            changed += 1
    print(f"backfilled metadata for {changed} of {len(files)} cassette files")
    return 0


def run_suite(suite: str, trace: Path) -> bool:
    environment = dict(os.environ)
    environment.pop("CODEREX_CASSETTE_MODE", None)
    environment["CODEREX_CASSETTE_TRACE"] = str(trace)
    for binary, name_filter in SUITES[suite]:
        command = ["cargo", "test", "-p", "coderex-cli", "--test", binary]
        if name_filter:
            command.append(name_filter)
        command.extend(["--", "--test-threads=1"])
        print(f"  replay {binary}" + (f" {name_filter}" if name_filter else ""))
        result = subprocess.run(
            command,
            cwd=ROOT,
            env=environment,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            print(
                f"error: {binary} failed; not pruning {suite}\n{result.stderr[-2000:]}",
                file=sys.stderr,
            )
            return False
    return True


def prune(args: argparse.Namespace) -> int:
    removed = 0
    missing: list[str] = []
    for suite in sorted(SUITES):
        directory = CASSETTES / suite
        if not directory.is_dir():
            continue
        print(f"tracing {suite}")
        with tempfile.NamedTemporaryFile("w+", suffix=".trace", delete=False) as handle:
            trace = Path(handle.name)
        try:
            if not run_suite(suite, trace):
                continue
            text = trace.read_text(encoding="utf-8", errors="replace")
        finally:
            trace.unlink(missing_ok=True)
        used = set(re.findall(r"key=([0-9a-fA-F]+)", text))
        if not used:
            print(f"warning: no cassette requests traced for {suite}; skipped", file=sys.stderr)
            continue
        files = {path.stem: path for path in directory.glob("*.json")}
        marker_keys = {
            path.stem.removeprefix("_missing_")
            for path in directory.glob("_missing_*.json")
        }
        for path in directory.glob("_missing_*.json"):
            if args.dry_run:
                print(f"would delete {path.relative_to(ROOT)}")
            else:
                path.unlink()
                removed += 1
        for key, path in sorted(files.items()):
            if key in used or key.startswith("_missing_"):
                continue
            if args.dry_run:
                print(f"would delete {path.relative_to(ROOT)}")
            else:
                path.unlink()
                removed += 1
        for key in sorted(used - set(files) - marker_keys):
            missing.append(f"{suite}/{key}.json")
        print(f"  {len(used)} keys used, {len(files)} files")
    for path in missing:
        print(f"error: trace used a cassette that does not exist: {path}", file=sys.stderr)
    if args.dry_run:
        print("dry run: no files were deleted")
    else:
        print(f"pruned {removed} cassette files")
    return 1 if missing else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    check_parser = subparsers.add_parser("check")
    check_parser.add_argument(
        "--max-age-days",
        type=int,
        default=int(os.environ.get("CASSETTE_MAX_AGE_DAYS", "180")),
    )
    check_parser.add_argument(
        "--max-mb", type=int, default=int(os.environ.get("CASSETTE_MAX_MB", "30"))
    )
    check_parser.add_argument("--strict", action="store_true")
    check_parser.set_defaults(func=check)

    backfill_parser = subparsers.add_parser("backfill")
    backfill_parser.add_argument(
        "--no-git-date",
        action="store_true",
        help="use file mtime instead of the file's most recent git commit date",
    )
    backfill_parser.set_defaults(func=backfill)

    prune_parser = subparsers.add_parser("prune")
    prune_parser.add_argument("--dry-run", action="store_true")
    prune_parser.set_defaults(func=prune)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
