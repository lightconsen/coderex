#!/usr/bin/env python3
"""Long-session soak driver.

Runs the real `coderex` TUI in a pseudo-terminal against the static provider,
submits N prompts that each perform one Bash tool call, and samples resident
memory, open file descriptors and the session directory. The provider emits a
unique marker per turn, so a turn is complete when that marker appears rather
than after a fixed sleep.

Environment:
  SOAK_TURNS            number of turns (default 200)
  SOAK_TURN_TIMEOUT     seconds allowed per turn (default 120)
  SOAK_RSS_GROWTH       allowed RSS growth between warmup and the last sample
                        as a fraction (default 0.20)
  SOAK_FD_GROWTH        allowed file-descriptor increase (default 16)
  SOAK_OUTPUT_DIR       report directory (default target/soak)

Exit status is 1 when a turn times out, the process dies, RSS or descriptor
growth exceeds the limits, or the final `/exit` does not terminate the TUI.
"""

from __future__ import annotations

import csv
import fcntl
import json
import os
import pty
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target/debug/coderex"
PROVIDER = ROOT / "target/debug/static-anthropic-server"
RESPONSES = ROOT / "tests/responses.json"

TURNS = int(os.environ.get("SOAK_TURNS", "200"))
TURN_TIMEOUT = float(os.environ.get("SOAK_TURN_TIMEOUT", "120"))
RSS_GROWTH = float(os.environ.get("SOAK_RSS_GROWTH", "0.20"))
FD_GROWTH = int(os.environ.get("SOAK_FD_GROWTH", "16"))
WARMUP = min(10, max(1, TURNS // 10))
OUT = ROOT / os.environ.get("SOAK_OUTPUT_DIR", "target/soak")


def build() -> None:
    subprocess.run(
        [
            "cargo",
            "build",
            "--bin",
            "coderex",
            "--bin",
            "static-anthropic-server",
        ],
        cwd=ROOT,
        check=True,
    )


def start_provider() -> tuple[subprocess.Popen[str], str]:
    provider = subprocess.Popen(
        [
            str(PROVIDER),
            "--port",
            "0",
            "--responses",
            str(RESPONSES),
        ],
        cwd=ROOT,
        stderr=subprocess.PIPE,
        text=True,
    )
    assert provider.stderr is not None
    deadline = time.time() + 15
    while time.time() < deadline:
        line = provider.stderr.readline()
        if not line:
            break
        if "listening on " in line:
            address = line.split("listening on ", 1)[1].strip()
            # Keep draining the provider log: it writes one line per request
            # and an unread pipe would eventually block it.
            threading.Thread(
                target=lambda: [None for _ in provider.stderr], daemon=True
            ).start()
            return provider, f"http://{address}"
    provider.kill()
    raise SystemExit("static provider did not announce its address")


def session_dir_size(path: Path) -> int:
    total = 0
    if not path.exists():
        return 0
    for entry in path.rglob("*"):
        try:
            if entry.is_file():
                total += entry.stat().st_size
        except OSError:
            pass
    return total


def rss_kb(pid: int) -> int:
    result = subprocess.run(
        ["ps", "-o", "rss=", "-p", str(pid)],
        capture_output=True,
        text=True,
        check=False,
    )
    try:
        return int(result.stdout.strip())
    except ValueError:
        return 0


def fd_count(pid: int) -> int:
    proc = Path(f"/proc/{pid}/fd")
    if proc.is_dir():
        return len(list(proc.iterdir()))
    result = subprocess.run(
        ["lsof", "-nP", "-p", str(pid)],
        capture_output=True,
        text=True,
        check=False,
    )
    return max(0, len(result.stdout.splitlines()) - 1)


def wait_for_exit(pid: int, fd: int, timeout: float) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            reaped, _ = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            return True
        if reaped == pid:
            return True
        ready, _, _ = select.select([fd], [], [], 0.1)
        if ready:
            try:
                os.read(fd, 65536)
            except OSError:
                pass
        time.sleep(0.05)
    return False


def set_winsize(fd: int, rows: int = 40, cols: int = 120) -> None:
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def read_until(
    fd: int,
    buffer: str,
    needle: str,
    deadline: float,
) -> tuple[str, bool]:
    while time.time() < deadline:
        if needle in buffer:
            return buffer, True
        ready, _, _ = select.select([fd], [], [], 0.2)
        if not ready:
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            return buffer, False
        if not chunk:
            return buffer, False
        buffer += chunk.decode("utf-8", "replace")
    return buffer, needle in buffer


def main() -> int:
    OUT.mkdir(parents=True, exist_ok=True)
    build()
    provider, base_url = start_provider()
    work = Path(tempfile.mkdtemp(prefix="coderex-soak-"))
    home = Path(tempfile.mkdtemp(prefix="coderex-soak-home-"))
    (work / "src").mkdir()
    (work / "src/lib.rs").write_text("pub fn add(a: i32, b: i32) -> i32 { a + b }\n")
    (home / "settings.json").write_text(
        json.dumps({"config_version": 4, "telemetry_enabled": False})
    )

    env = dict(os.environ)
    env.update(
        {
            "ANTHROPIC_API_KEY": "mock",
            "ANTHROPIC_BASE_URL": base_url,
            "ANTHROPIC_MODEL": "claude-sonnet-4-6",
            "CLAUDE_CONFIG_DIR": str(home),
            "CODEREX_LOG": "off",
            "TERM": "xterm-256color",
        }
    )
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(work)
        os.execvpe(
            str(BIN),
            [str(BIN), "--dangerously-skip-permissions", "--no-chrome"],
            env,
        )

    set_winsize(fd)
    output = ""
    samples: list[dict[str, object]] = []
    failure: str | None = None
    try:
        output, ready = read_until(fd, output, "CodeRex", time.time() + 30)
        if not ready:
            raise SystemExit("coderex did not render its startup banner")

        for turn in range(1, TURNS + 1):
            marker = f"soak-turn-ok-{turn}"
            os.write(fd, f"soak turn {turn}\r".encode())
            deadline = time.time() + TURN_TIMEOUT
            output, ready = read_until(fd, output, marker, deadline)
            if not ready:
                failure = f"turn {turn} did not finish within {TURN_TIMEOUT}s"
                break
            if turn % 10 == 0 or turn == 1 or turn == TURNS:
                samples.append(
                    {
                        "turn": turn,
                        "rss_kb": rss_kb(pid),
                        "fds": fd_count(pid),
                        "session_bytes": session_dir_size(work / ".coderex"),
                    }
                )

        os.write(fd, b"/exit\r")
        if not wait_for_exit(pid, fd, 30):
            os.kill(pid, 15)
            failure = failure or "the TUI did not exit after /exit"
        os.close(fd)
        fd = -1
    finally:
        if fd >= 0:
            try:
                os.close(fd)
            except OSError:
                pass
        provider.kill()
        provider.wait()
        if failure:
            log = OUT / "session.log"
            log.write_text(output[-200_000:], encoding="utf-8", errors="replace")
            print(f"session log: {log}", file=sys.stderr)

    summary: dict[str, object] = {
        "turns": TURNS,
        "completed_turns": samples[-1]["turn"] if samples else 0,
        "samples": samples,
        "failure": failure,
    }
    if failure:
        summary["status"] = "failed"
    else:
        warm = samples[len(samples) // 5] if len(samples) >= 5 else samples[0]
        last = samples[-1]
        first_rss = int(warm["rss_kb"])
        last_rss = int(last["rss_kb"])
        rss_growth = (last_rss - first_rss) / first_rss if first_rss else 0.0
        fd_growth = int(last["fds"]) - int(warm["fds"])
        summary.update(
            {
                "rss_growth": rss_growth,
                "fd_growth": fd_growth,
                "session_bytes": last["session_bytes"],
            }
        )
        problems = []
        if rss_growth > RSS_GROWTH:
            problems.append(
                f"RSS grew {rss_growth:.1%} (limit {RSS_GROWTH:.0%})"
            )
        if fd_growth > FD_GROWTH:
            problems.append(f"fds grew by {fd_growth} (limit {FD_GROWTH})")
        if problems:
            summary["status"] = "failed"
            summary["problems"] = problems
        else:
            summary["status"] = "ok"

    (OUT / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    with (OUT / "samples.csv").open("w", newline="") as handle:
        writer = csv.DictWriter(
            handle, fieldnames=["turn", "rss_kb", "fds", "session_bytes"]
        )
        writer.writeheader()
        writer.writerows(samples)

    print(json.dumps(summary, indent=2))
    shutil.rmtree(work, ignore_errors=True)
    shutil.rmtree(home, ignore_errors=True)
    return 0 if summary["status"] == "ok" else 1


if __name__ == "__main__":
    sys.exit(main())
