#!/usr/bin/env python3
"""Check that every release archive contains the binary it is named for.

Called by the release workflow after the build matrix has uploaded its
artifacts. A tarball that unpacks to nothing — a packaging step that silently
copied the wrong path — would otherwise be published as a working release.

Usage: scripts/check-release-archives.py [directory]

The directory defaults to `artifacts`. Archives are `coderex-<target>.tar.gz`
or `coderex-<target>.zip`, and must contain `coderex-<target>` (`.exe` on
Windows). Anything else in the directory is ignored.
"""

from __future__ import annotations

import pathlib
import sys
import tarfile
import zipfile


def archive_names(path: pathlib.Path) -> list[str]:
    if path.name.endswith(".tar.gz"):
        with tarfile.open(path) as handle:
            return handle.getnames()
    if path.name.endswith(".zip"):
        with zipfile.ZipFile(path) as handle:
            return handle.namelist()
    raise ValueError(f"{path.name} is not a supported archive")


def expected_member(name: str) -> str:
    stem = name.removesuffix(".tar.gz").removesuffix(".zip")
    target = stem.removeprefix("coderex-")
    member = f"coderex-{target}"
    return member + ".exe" if name.endswith(".zip") else member


def main(argv: list[str]) -> int:
    directory = pathlib.Path(argv[1] if len(argv) > 1 else "artifacts")
    if not directory.is_dir():
        print(f"no release artifacts at {directory}", file=sys.stderr)
        return 1

    checked = 0
    failures = 0
    for path in sorted(directory.iterdir()):
        if not path.name.startswith("coderex-"):
            continue
        if not (path.name.endswith(".tar.gz") or path.name.endswith(".zip")):
            continue
        try:
            names = archive_names(path)
        except (tarfile.TarError, zipfile.BadZipFile, ValueError) as error:
            print(f"{path.name}: unreadable ({error})", file=sys.stderr)
            failures += 1
            continue
        expected = expected_member(path.name)
        if expected not in names:
            print(f"{path.name}: missing {expected} (has {names})", file=sys.stderr)
            failures += 1
            continue
        checked += 1
        print(f"{path.name}: {expected}")

    if checked == 0 and failures == 0:
        print(f"no release archives found in {directory}", file=sys.stderr)
        return 1
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
