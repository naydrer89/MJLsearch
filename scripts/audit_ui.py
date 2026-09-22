#!/usr/bin/env python3
"""Audits the dashboard's rendering across real viewports.

Layout defects are invisible in source: a grid that is fine at 1440px silently
pushes its last column off a 390px screen, and two positioned boxes overlap only at
some widths. The only honest way to find them is to render the page at each
viewport and measure what the browser actually laid out. This script does that, and
it is kept in the repository rather than thrown away because the next CSS change
deserves the same check.

The measuring itself lives in ``audit_ui.js``, which this evaluates in the page.
Findings reported to the user are the same ones, phrased for a human: horizontal
overflow, overlaps, undersized tap targets, unreadably small or low-contrast text,
and content clipped by an ancestor that hides it.

Usage::

    uv run python scripts/audit_ui.py [--url http://127.0.0.1:5000] [--query rust]

Exit status is 1 when any finding is reported, so it can gate a build.
"""

from __future__ import annotations

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from playwright.sync_api import Page, sync_playwright

# The range rather than a representative sample of it: a layout can break at 320 as
# easily as at 1920, and those are the two widths nobody opens while developing.
# 390 is the narrowest common iPhone logical width.
VIEWPORTS: dict[str, tuple[int, int]] = {
    "wide": (1920, 1080),
    "desktop": (1440, 900),
    "tablet": (834, 1112),
    "mobile": (390, 844),
    "small": (320, 568),
}

# Reports are capped per kind so one broken component cannot produce a hundred
# identical lines and bury everything else.
MAX_FINDINGS_PER_KIND = 12

AUDIT_JS = Path(__file__).with_name("audit_ui.js")


@dataclass
class Finding:
    """One defect, tagged with the viewport it showed up at."""

    viewport: str
    kind: str
    detail: str

    def __str__(self) -> str:
        return f"{self.viewport:<8} {self.kind:<12} {self.detail}"


def collect_findings(name: str, raw: dict[str, Any]) -> list[Finding]:
    """Turns the page's measurements into human-readable findings."""
    findings: list[Finding] = []

    if raw["pageOverflow"] > 1:
        findings.append(
            Finding(name, "overflow", f"page scrolls sideways by {raw['pageOverflow']}px")
        )

    def add(kind: str, entries: list[dict[str, Any]], render: Any) -> None:
        for entry in entries[:MAX_FINDINGS_PER_KIND]:
            findings.append(Finding(name, kind, render(entry)))

    add(
        "off-screen",
        raw["overflow"],
        lambda e: f"{e['el']} extends {e['overflowPx']}px past the edge",
    )
    add("overlap", raw["overlaps"], lambda e: f"{e['a']} overlaps {e['b']} by {e['overlap']}")
    add("tap-target", raw["tapTargets"], lambda e: f"{e['el']} is {e['size']}")
    add("tiny-text", raw["tinyText"], lambda e: f"{e['el']} is {e['fontSize']}px")
    add(
        "contrast",
        raw["contrast"],
        lambda e: f"{e['el']} is {e['ratio']}:1, needs {e['required']}:1 at {e['fontSize']}px",
    )
    add(
        "clipped",
        raw["clipped"],
        lambda e: f"{e['el']} is cut off by {e['clippedBy']} ({e['axis']})",
    )

    return findings


def audit_viewport(page: Page, url: str, name: str, width: int, height: int) -> list[Finding]:
    """Renders `url` at one viewport and returns everything wrong with it."""
    page.set_viewport_size({"width": width, "height": height})
    page.goto(url, wait_until="networkidle")
    # Let the polling script and its count-up animation settle, so measurements are
    # taken against a stable layout rather than a mid-transition one.
    page.wait_for_timeout(1200)

    raw: dict[str, Any] = page.evaluate(AUDIT_JS.read_text(encoding="utf-8"))
    return collect_findings(name, raw)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:5000")
    parser.add_argument(
        "--query",
        default="tantivy indexing",
        help="Query to render the results path with; empty string skips it.",
    )
    parser.add_argument("--shots", default="/tmp/ui-audit", help="Where to write screenshots.")
    parser.add_argument("--json", action="store_true", help="Emit findings as JSON.")
    args = parser.parse_args()

    shots = Path(args.shots)
    shots.mkdir(parents=True, exist_ok=True)

    url = args.url
    if args.query:
        url = f"{args.url}/?q={args.query.replace(' ', '+')}"

    every: list[Finding] = []

    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True)
        for name, (width, height) in VIEWPORTS.items():
            # A coarse pointer at phone width, so touch sizing is exercised the way
            # a phone would rather than as a narrow desktop.
            context = browser.new_context(
                viewport={"width": width, "height": height},
                has_touch=name in {"mobile", "small"},
                is_mobile=name in {"mobile", "small"},
                device_scale_factor=1,
            )
            page = context.new_page()
            findings = audit_viewport(page, url, name, width, height)
            every.extend(findings)

            page.screenshot(path=str(shots / f"{name}.png"))
            page.screenshot(path=str(shots / f"{name}-full.png"), full_page=True)
            print(f"{name:<8} {width}x{height}  {len(findings)} finding(s)", file=sys.stderr)
            context.close()
        browser.close()

    if args.json:
        print(json.dumps([finding.__dict__ for finding in every], indent=2))
    else:
        for finding in every:
            print(finding)

    print(f"\n{len(every)} finding(s) across {len(VIEWPORTS)} viewports", file=sys.stderr)
    print(f"screenshots in {shots}", file=sys.stderr)
    return 1 if every else 0


if __name__ == "__main__":
    raise SystemExit(main())
