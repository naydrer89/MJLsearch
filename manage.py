#!/usr/bin/env python3
"""Manage the MJLsearch stack from one file: start, stop, status, logs.

Written for a server, so it assumes nothing beyond the project itself:

    python3 manage.py start                 # build if needed, start api+ui+dashboard
    python3 manage.py start --crawl         # ... and crawl rounds in the background
    python3 manage.py start --api-port 8100 # pick ports explicitly (auto-picks if taken)
    python3 manage.py status                # what is up, on which port, answering?
    python3 manage.py stop                  # stop everything this tool started
    python3 manage.py logs api              # tail a service log (api|ui|dashboard|crawl)

Everything it starts is detached (its own session), logs to logs/<name>.log and
records its pid in logs/<name>.pid, so `stop` is exact: it kills only what this
tool started, never a process that happens to hold a port. A second `start` is
idempotent -- services that already answer are reused, not restarted.

Stdlib only, so it runs before `uv sync` has produced anything.
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

# name -> (script, default port, url path that proves it answers)
SERVICES = {
    "api": ("scripts/run_api.sh", 8000, "/health"),
    "ui": ("scripts/run_searchui.sh", 5001, "/"),
    "dashboard": ("scripts/run_dashboard.sh", 5000, "/"),
}
LOG_NAMES = sorted(SERVICES) + ["crawl"]


def have(cmd: str) -> bool:
    """True when `cmd` resolves on PATH."""
    return subprocess.run(
        ["bash", "-lc", f"command -v {cmd} >/dev/null 2>&1"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ).returncode == 0


def port_busy(port: int, host: str = "127.0.0.1") -> bool:
    """True when something already listens on `port`."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.settimeout(0.3)
        return sock.connect_ex((host, port)) == 0


def pick_port(wanted: int, host: str = "127.0.0.1") -> int:
    """`wanted` if free, otherwise the next free port above it."""
    if not port_busy(wanted, host):
        return wanted
    probe = wanted
    for _ in range(100):
        probe += 1
        if not port_busy(probe, host):
            return probe
    raise SystemExit(f"no free port found above {wanted}")


def pid_of(name: str) -> int | None:
    """Recorded pid for `name`, only if that process is still alive."""
    path = LOGS / f"{name}.pid"
    try:
        pid = int(path.read_text().strip())
    except (OSError, ValueError):
        return None
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        path.unlink(missing_ok=True)
        return None
    except PermissionError:
        return pid
    return pid


def answers(port: int, path: str, host: str = "127.0.0.1", timeout: float = 2.0) -> bool:
    """True when http://host:port/path answers with any status at all."""
    try:
        req = urllib.request.Request(f"http://{host}:{port}{path}")
        urllib.request.urlopen(req, timeout=timeout)
        return True
    except urllib.error.HTTPError:
        return True  # a 404 still proves the server is up
    except (urllib.error.URLError, TimeoutError, OSError):
        return False


def spawn(name: str, cmd: list[str], env: dict[str, str]) -> int:
    """Detach `cmd` into its own session, log and record its pid."""
    LOGS.mkdir(exist_ok=True)
    log = open(LOGS / f"{name}.log", "ab", buffering=0)
    stamp = f"\n=== {time.strftime('%Y-%m-%d %H:%M:%S')} manage.py start {name} ===\n"
    log.write(stamp.encode())
    proc = subprocess.Popen(
        cmd,
        cwd=ROOT,
        env=env,
        stdout=log,
        stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL,
        start_new_session=True,  # survives this CLI and its terminal
    )
    (LOGS / f"{name}.pid").write_text(f"{proc.pid}\n")
    return proc.pid


def env_for(extra: dict[str, str]) -> dict[str, str]:
    merged = os.environ.copy()
    merged.update(extra)
    return merged


def ensure_built() -> None:
    """Build the Rust extension and binaries when they are missing or stale.

    build.sh forces the extension rebuild (--reinstall-package), which is what
    makes source edits visible; a plain `uv sync` would silently reuse yesterday's
    cached wheel.
    """
    if not have("uv") or not have("cargo"):
        raise SystemExit(
            "uv and cargo are required but not on PATH.\n"
            "Run ./start.sh once (it installs both into the user's home),\n"
            "or on a fresh Ubuntu server: sudo ./deploy/setup-server.sh"
        )
    print("==> building (release)")
    subprocess.run(["bash", "scripts/build.sh"], cwd=ROOT, check=True)


def wait_up(name: str, port: int, path: str, seconds: float = 60.0) -> bool:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if answers(port, path):
            return True
        pid = pid_of(name)
        if pid is None:
            return False  # died while starting -- logs will say why
        time.sleep(0.5)
    return False


def cmd_start(args: argparse.Namespace) -> None:
    host = args.host
    ports = {
        "api": args.api_port or 8000,
        "ui": args.ui_port or 5001,
        "dashboard": args.dashboard_port or 5000,
    }

    if args.build or not (ROOT / ".venv").exists():
        ensure_built()

    backend = f"http://{host}:{ports['api']}"
    plan = {
        "api": {"SEARCH_HOST": host, "SEARCH_PORT": str(ports["api"])},
        "ui": {
            "SEARCH_HOST": host,
            "SEARCH_UI_PORT": str(ports["ui"]),
            "SEARCH_BACKEND_URL": backend,
        },
        "dashboard": {
            "SEARCH_HOST": host,
            "SEARCH_DASHBOARD_PORT": str(ports["dashboard"]),
            "SEARCH_BACKEND_URL": backend,
            # A server should not beep at whoever sshs in; opt back in with
            # SEARCH_STARTUP_SOUND=1.
            "SEARCH_STARTUP_SOUND": "0",
        },
    }

    started: list[tuple[str, int]] = []
    for name, extra in plan.items():
        script, default_port, probe = SERVICES[name]
        wanted = ports[name]
        if answers(wanted, probe, host):
            print(f"    {name}: already answering on :{wanted}, reused")
            continue
        if port_busy(wanted, host):
            if not args.pick:
                raise SystemExit(
                    f"port {wanted} is taken by something else "
                    f"(use --pick to auto-select a free port)"
                )
            ports[name] = wanted = pick_port(wanted, host)
            # The ui and dashboard point at the api by URL, so only their own
            # listen ports change; the api's port is fixed before this loop.
            if name == "ui":
                extra["SEARCH_UI_PORT"] = str(wanted)
            elif name == "dashboard":
                extra["SEARCH_DASHBOARD_PORT"] = str(wanted)
            print(f"    {name}: :{default_port} busy, picked :{wanted}")
        if name == "ui" or name == "dashboard":
            extra["SEARCH_BACKEND_URL"] = f"http://{host}:{ports['api']}"
        pid = spawn(name, ["bash", script], env_for(extra))
        print(f"    {name}: started pid {pid} on :{wanted}")
        started.append((name, wanted))

    for name, port in started:
        _, _, probe = SERVICES[name]
        if wait_up(name, port, probe):
            print(f"    {name}: up on http://{host}:{port}")
        else:
            print(f"    {name}: NOT answering yet -- see logs/{name}.log", file=sys.stderr)

    if args.crawl:
        pid = spawn(
            "crawl",
            ["bash", "scripts/crawl_official.sh", "--loop", "20"],
            env_for({"RUST_LOG": "info"}),
        )
        print(f"    crawl: started pid {pid} (loop, 20s between rounds)")

    print("\nstatus:")
    cmd_status(
        argparse.Namespace(
            host=host,
            api_port=ports["api"],
            ui_port=ports["ui"],
            dashboard_port=ports["dashboard"],
            json_output=False,
        )
    )


def cmd_stop(args: argparse.Namespace) -> None:
    names = list(SERVICES) + (["crawl"] if not args.no_crawl else [])
    stopped_any = False
    for name in names:
        pid = pid_of(name)
        if pid is None:
            continue
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        # Crawl rounds can hold RocksDB writes; give them a moment to exit
        # cleanly before falling back to SIGKILL.
        grace = 20 if name == "crawl" else 8
        deadline = time.monotonic() + grace
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.3)
        else:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        (LOGS / f"{name}.pid").unlink(missing_ok=True)
        print(f"    {name}: stopped (pid {pid})")
        stopped_any = True
    if not stopped_any:
        print("nothing to stop: no live pid files")


def cmd_status(args: argparse.Namespace) -> None:
    report = {}
    for name, (_, default_port, probe) in SERVICES.items():
        pid = pid_of(name)
        port = getattr(args, f"{name}_port", None) or default_port
        up = answers(port, probe, args.host)
        report[name] = {"pid": pid, "port": port, "answering": up}
    crawl_pid = pid_of("crawl")
    report["crawl"] = {"pid": crawl_pid, "port": None, "answering": crawl_pid is not None}

    if args.json_output:
        print(json.dumps(report, indent=2))
        return

    for name, info in report.items():
        state = "up" if info["answering"] else "down"
        port_part = f" on :{info['port']}" if info["port"] else ""
        pid_part = f", pid {info['pid']}" if info["pid"] else ""
        marker = "" if info["answering"] == (info["pid"] is not None) else "  <-- unexpected"
        print(f"    {name:<9} {state}{port_part}{pid_part}{marker}")
        if info["answering"] and info["pid"] is None:
            print("              (answering, but not started by manage.py)")

    # Index size is the one number that shows the system is alive and growing.
    if report["api"]["answering"]:
        try:
            with urllib.request.urlopen(
                f"http://{args.host}:{report['api']['port']}/analytics/overview", timeout=3
            ) as resp:
                body = json.loads(resp.read().decode())
            docs = body["index"]["doc_count"]
            queued = body.get("crawler", {}).get("queued")
            line = f"    index     {docs} documents"
            if queued is not None:
                line += f", {queued} queued in the frontier"
            print(line)
        except (urllib.error.URLError, ValueError, KeyError, TypeError):
            pass


def cmd_logs(args: argparse.Namespace) -> None:
    path = LOGS / f"{args.service}.log"
    if not path.exists():
        raise SystemExit(f"no log at {path}")
    cmd = ["tail", "-n", str(args.lines), "-f"] if args.follow else ["tail", "-n", str(args.lines)]
    subprocess.run(cmd + [str(path)])


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="manage.py", description="Start, stop and inspect the MJLsearch stack."
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_start = sub.add_parser("start", help="build if needed and start api, ui, dashboard")
    p_start.add_argument("--host", default="127.0.0.1", help="bind host (0.0.0.0 for a server)")
    p_start.add_argument("--api-port", type=int, help="api port (default 8000)")
    p_start.add_argument("--ui-port", type=int, help="search page port (default 5001)")
    p_start.add_argument("--dashboard-port", type=int, help="dashboard port (default 5000)")
    p_start.add_argument("--pick", action="store_true", help="auto-pick a free port when busy")
    p_start.add_argument("--crawl", action="store_true", help="also run crawl rounds in the background")
    p_start.add_argument("--no-build", dest="build", action="store_false", help="skip the build step")
    p_start.set_defaults(build=True, func=cmd_start)

    p_stop = sub.add_parser("stop", help="stop what manage.py started")
    p_stop.add_argument("--no-crawl", action="store_true", help="leave the crawl running")
    p_stop.set_defaults(func=cmd_stop)

    p_status = sub.add_parser("status", help="show what is running and answering")
    p_status.add_argument("--host", default="127.0.0.1")
    p_status.add_argument("--api-port", dest="api_port", type=int)
    p_status.add_argument("--ui-port", dest="ui_port", type=int)
    p_status.add_argument("--dashboard-port", dest="dashboard_port", type=int)
    p_status.add_argument("--json", dest="json_output", action="store_true")
    p_status.set_defaults(func=cmd_status)

    p_logs = sub.add_parser("logs", help="tail a service log (api|ui|dashboard|crawl)")
    p_logs.add_argument("service", choices=LOG_NAMES)
    p_logs.add_argument("-n", "--lines", type=int, default=50)
    p_logs.add_argument("-f", "--follow", action="store_true")
    p_logs.set_defaults(func=cmd_logs)

    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
