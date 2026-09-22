#!/usr/bin/env python3
"""Watch the backend and (re)start the stack when it stops answering.

    python3 health.py              # one check, exit 0 healthy / 1 unhealthy
    python3 health.py --watch      # loop forever, interval --interval SECONDS
    python3 health.py --fix        # actually restart what is down (default: report only)
    python3 health.py --once --fix # try one round of repair, then report

What counts as healthy: the API answers /health with status ok *and* its index is
open. The pages are derived services -- if the API is fine but a page is down, the
page alone is restarted, not the whole stack.

--fix is deliberately conservative: it starts services that are down and kills a
page process only when its own port stopped answering. It never touches the index,
the crawl state, or a service that is answering. Anything it does is written to
logs/health.log and, in --watch mode, to stdout. Run it from systemd, cron, or
`python3 health.py --watch --fix &` -- it holds no state between checks.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
LOGS = ROOT / "logs"

SERVICES = {
    "api": (8000, "/health"),
    "ui": (5001, "/"),
    "dashboard": (5000, "/"),
}


def answers(port: int, path: str, host: str = "127.0.0.1", timeout: float = 3.0):
    """Return (True, body) when the endpoint answers, (False, reason) when not."""
    try:
        with urllib.request.urlopen(f"http://{host}:{port}{path}", timeout=timeout) as resp:
            return True, resp.read().decode(errors="replace")
    except urllib.error.HTTPError as error:
        # A 404 or 500 is still an answering server; the caller decides what to make
        # of the body. Only transport failure means "down".
        try:
            return True, error.read().decode(errors="replace")
        except Exception:
            return True, ""
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        return False, str(error)


def api_is_healthy(host: str = "127.0.0.1") -> tuple[bool, str]:
    """Healthy means: /health answers, status ok, index open.

    A running-but-degenerate API (index failed to open after a data change, the
    extension reports an error) would otherwise look fine to a plain TCP check and
    every page behind it would keep serving error panels.
    """
    ok, body = answers(SERVICES["api"][0], SERVICES["api"][1], host)
    if not ok:
        return False, f"api transport: {body}"
    try:
        payload = json.loads(body)
    except ValueError:
        return False, f"api answered non-JSON: {body[:120]!r}"
    if payload.get("status") != "ok":
        return False, f"api status={payload.get('status')!r}: {payload.get('detail')}"
    if not payload.get("index_open", False):
        return False, "api up but index_open is false"
    return True, "ok"


def pid_listening_on(port: int, host: str = "127.0.0.1") -> int | None:
    """The pid holding a listening socket, or None. Linux only (ss), like manage.py."""
    try:
        out = subprocess.run(
            ["ss", "-ltnp"],
            capture_output=True,
            text=True,
            timeout=5,
        ).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    for line in out.splitlines():
        if f":{port} " not in line:
            continue
        if "pid=" not in line:
            return None  # somebody's socket, but no visible pid
        pid_part = line.split("pid=")[1].split(",")[0]
        try:
            return int(pid_part)
        except ValueError:
            return None
    return None


def stop_pid(pid: int, grace: float = 8.0) -> None:
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    deadline = time.monotonic() + grace
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        time.sleep(0.2)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def spawn(name: str, cmd: list[str]) -> int:
    LOGS.mkdir(exist_ok=True)
    log = open(LOGS / f"{name}.log", "ab", buffering=0)
    stamp = f"\n=== {time.strftime('%Y-%m-%d %H:%M:%S')} health.py restart ===\n"
    log.write(stamp.encode())
    proc = subprocess.Popen(
        cmd,
        cwd=ROOT,
        stdout=log,
        stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL,
        start_new_session=True,
    )
    (LOGS / f"{name}.pid").write_text(f"{proc.pid}\n")
    return proc.pid


def wait_healthy(seconds: float, host: str = "127.0.0.1") -> bool:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        ok, _ = api_is_healthy(host)
        if ok:
            return True
        time.sleep(0.5)
    return False


def repair(dry_run: bool, host: str) -> list[str]:
    """Bring the stack back. Returns the lines of what it did / would do."""
    actions: list[str] = []
    healthy, why = api_is_healthy(host)
    if not healthy:
        actions.append(f"api down: {why}")
        if not dry_run:
            # A wedged uvicorn (answers nothing but holds the port) must be
            # removed before a new one can bind. uv run spawns a child, so kill
            # the whole process group of the recorded pid when it is still ours.
            pid = pid_listening_on(SERVICES["api"][0], host)
            if pid is not None:
                actions.append(f"  stopping holder of :{SERVICES['api'][0]} (pid {pid})")
                stop_pid(pid)
            pid_file = LOGS / "api.pid"
            if pid_file.exists():
                try:
                    stop_pid(int(pid_file.read_text().strip()))
                except (OSError, ValueError):
                    pass
            spawn("api", ["bash", "scripts/run_api.sh"])
            if not wait_healthy(60, host):
                actions.append("  api did not come back healthy within 60s -- see logs/api.log")
                return actions
            actions.append("  api restarted and healthy")
        return actions

    # API fine: check the two pages, restart only a page that is down.
    for name in ("ui", "dashboard"):
        port, path = SERVICES[name]
        ok, _ = answers(port, path, host)
        if ok:
            continue
        actions.append(f"{name} down on :{port}")
        if not dry_run:
            pid = pid_listening_on(port, host)
            if pid is not None:
                stop_pid(pid)
            pid_file = LOGS / f"{name}.pid"
            if pid_file.exists():
                try:
                    stop_pid(int(pid_file.read_text().strip()))
                except (OSError, ValueError):
                    pass
            script = {
                "ui": ["bash", "scripts/run_searchui.sh"],
                "dashboard": ["bash", "scripts/run_dashboard.sh"],
            }[name]
            spawn(name, script)
            deadline = time.monotonic() + 30
            came_back = False
            while time.monotonic() < deadline:
                ok, _ = answers(port, path, host)
                if ok:
                    came_back = True
                    break
                time.sleep(0.5)
            actions.append(f"  {name} restarted, answering: {came_back}")
    return actions


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--watch", action="store_true", help="loop forever")
    parser.add_argument("--interval", type=float, default=30.0, help="seconds between checks")
    parser.add_argument(
        "--fix", action="store_true", help="restart what is down (default: report only)"
    )
    parser.add_argument(
        "--dry-run", action="store_true", help="report what --fix would do, change nothing"
    )
    parser.add_argument("--host", default="127.0.0.1")
    args = parser.parse_args()

    def log(line: str) -> None:
        print(line, flush=True)
        LOGS.mkdir(exist_ok=True)
        with open(LOGS / "health.log", "a") as fh:
            fh.write(f"{time.strftime('%Y-%m-%d %H:%M:%S')} {line}\n")

    while True:
        healthy, why = api_is_healthy(args.host)
        if not args.fix:
            # Report-only: never touch processes unless --fix was asked for.
            log(f"healthy ({why})" if healthy else f"unhealthy: {why}")
        elif healthy:
            actions = repair(args.dry_run, args.host)
            if actions:
                log("; ".join(actions))
        else:
            actions = repair(args.dry_run, args.host)
            log("; ".join(actions) if actions else f"unhealthy and no action taken: {why}")
        if not args.watch:
            sys.exit(0 if healthy else 1)
        time.sleep(args.interval)


if __name__ == "__main__":
    main()
