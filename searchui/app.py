"""The search page service.

    uv run python -m searchui.app

Server-rendered rather than a JavaScript application, and that is a deliberate
choice rather than a shortcut:

* a query becomes a real URL, so it can be shared, bookmarked, reloaded and walked
  back through with the browser's own history -- all of which a client-side search
  page has to reimplement;
* the first result is visible in the first byte, instead of after a round trip that
  cannot start until the script has parsed;
* with JavaScript off, searching still works.

The upstream call goes through :func:`fetch_json`, so the timeout, the failure
shape and the "backend is down" state are decided in exactly one place and the
page can always say what is wrong instead of returning a 500 of its own.

Deliberately self-contained rather than importing the dashboard's helper: the two
are separate services, and a search page that cannot start because the monitoring
service was refactored is a worse trade than thirty duplicated lines.
"""

from __future__ import annotations

import logging
import math
import re
import time
from datetime import UTC, datetime
from html import escape, unescape
from typing import Any
from urllib.parse import unquote, urlsplit

import httpx
from flask import Flask, Response, jsonify, redirect, render_template, request, url_for
from markupsafe import Markup

from api.config import Settings, get_settings
from api.sound import play_startup_sound

logger = logging.getLogger(__name__)

# Identifies this service in the API's own logs, next to the queries it caused.
SEARCH_CLIENT = "mjlsearch-search-page"

# A page of results. Ten is what a screen holds without scrolling, and it is small
# enough that the extra candidates fetched for the per-domain cap stay cheap.
DEFAULT_PAGE_SIZE = 10

# Offered on the landing page. They are also the queries the integration tests
# drive, so the page advertises exactly what has been proven to work.
SUGGESTED_QUERIES = ("github", "amazon", "google", "rust")

# The one tag the snippet generator is allowed to produce.
BOLD_TAG = re.compile(r"(</?b>)")


def safe_snippet(markup: str | None) -> Markup:
    """Renders a snippet, keeping the `<b>` marks and escaping everything else.

    Tantivy hands back a fragment that is *already* escaped, with matched terms
    wrapped in `<b>`. So each text part is unescaped and then escaped again rather
    than passed through: the round trip turns an entity from the index into the
    character it stands for (`&quot;` becomes a real quote on screen, not the six
    letters), while still escaping whatever that character was.

    Passing the parts straight through would render entities as literal text, which
    is what this did first, and escaping them *without* unescaping first would
    double-escape every entity the index produced. A literal `<b>` in a page's own
    text arrives as `&lt;b&gt;`, survives the unescape as text, and is escaped again,
    so it stays literal rather than becoming a tag.
    """
    parts = BOLD_TAG.split(markup or "")
    return Markup(
        "".join(part if part in ("<b>", "</b>") else escape(unescape(part)) for part in parts)
    )


def breadcrumb(url: str) -> str:
    """A readable stand-in for a URL, in the shape a results page shows one."""
    parsed = urlsplit(url)
    host = parsed.netloc.removeprefix("www.")
    segments = [part for part in parsed.path.split("/") if part][:3]
    if not segments:
        return host or url
    return f"{host} › " + " › ".join(unquote(segment) for segment in segments)


def format_date(value: Any) -> str:
    """A fetched-at timestamp as a date, in UTC so it cannot be misread."""
    try:
        stamp = int(value)
    except (TypeError, ValueError):
        return "unknown"
    except OverflowError:
        return "unknown"
    return datetime.fromtimestamp(stamp, tz=UTC).strftime("%d %b %Y, %H:%M UTC")


def format_clock(value: Any) -> str:
    """A minute bucket as a wall-clock label, in UTC so it cannot be misread.

    UTC rather than local time on purpose: the bucket boundaries are UTC minutes
    written by the indexer, and rendering them in a reader's timezone would relabel
    the buckets instead of translating them.
    """
    try:
        stamp = int(value)
    except (TypeError, ValueError, OverflowError):
        return "unknown"
    return datetime.fromtimestamp(stamp, tz=UTC).strftime("%H:%M")


def format_age(seconds: Any) -> str:
    """An elapsed-time reading as a short phrase, or "never" when there is none."""
    if seconds is None:
        return "never"
    try:
        elapsed = float(seconds)
    except (TypeError, ValueError):
        return "never"
    if elapsed < 5:
        return "just now"
    if elapsed < 60:
        return f"{int(elapsed)} s ago"
    if elapsed < 3_600:
        return f"{int(elapsed // 60)} min ago"
    return f"{elapsed / 3_600:.1f} h ago"


def format_count(value: Any) -> str:
    """A number with thin spaces as thousands separators, like the result count."""
    try:
        return f"{int(value):,}".replace(",", "\u2009")
    except (TypeError, ValueError):
        return "0"


def decorate(results: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Adds the fields the template needs to each hit.

    Done here rather than in Jinja so the template stays a description of the page
    rather than a place where URL parsing has to be written and debugged.
    """
    decorated = []
    for hit in results:
        if not isinstance(hit, dict):
            continue
        enriched = dict(hit)
        enriched["breadcrumb"] = breadcrumb(str(hit.get("url") or ""))
        decorated.append(enriched)
    return decorated


def fetch_json(
    path: str,
    *,
    params: dict[str, Any] | None = None,
    settings: Settings | None = None,
) -> tuple[int, dict[str, Any]]:
    """Calls the API, returning ``(status, payload)`` and never raising.

    A transport failure comes back as 502 with a payload naming the backend and the
    reason. That is the state the page has to display, so it is returned as data
    rather than thrown: an exception here would turn the one page that can explain
    the outage into an error page.
    """
    resolved = settings if settings is not None else get_settings()

    try:
        with httpx.Client(
            base_url=resolved.backend_url,
            timeout=resolved.backend_timeout_seconds,
            headers={"x-request-id": SEARCH_CLIENT, "user-agent": SEARCH_CLIENT},
        ) as client:
            response = client.get(path, params=params)
    except httpx.HTTPError as error:
        logger.warning(
            "backend unreachable",
            extra={"backend": resolved.backend_url, "path": path, "detail": str(error)},
        )
        return 502, {
            "error": "backend unreachable",
            "backend": resolved.backend_url,
            "detail": str(error),
        }

    try:
        payload = response.json()
    except ValueError:
        return 502, {
            "error": "backend returned a non-JSON body",
            "backend": resolved.backend_url,
            "status": response.status_code,
        }

    return response.status_code, payload if isinstance(payload, dict) else {"data": payload}


# Minutes of history drawn on the stats page. One hour, matching the window the
# strip reports, and the same width the indexer keeps so the chart never asks for
# more than exists.
CHART_MINUTES = 60

# The plot's own coordinate space. The chart is drawn in these units and stretched
# onto the panel by the browser (`preserveAspectRatio="none"`), so the geometry never
# has to know a pixel width and cannot disagree with the box it lands in.
CHART_WIDTH = 1000.0
CHART_HEIGHT = 100.0
# Headroom at the top and the bottom. Without it the busiest minute's dot sits on the
# top edge of the box with half of itself outside, and a quiet window's flat line sits
# on the axis where it is indistinguishable from the axis.
CHART_PAD = 6.0


# How often the strip and the chart re-read the growth feed, in milliseconds. Five
# seconds is the compromise the panels are built around: fast enough that a running
# indexer visibly moves the number, slow enough that a page left open all day is not
# a load generator against the API it is watching.
GROWTH_POLL_MS = 5_000

# The shape a missing reading takes. Every field the templates read is present, so a
# down backend degrades the panels to zeros rather than to a template error.
NO_GROWTH: dict[str, Any] = {
    "available": False,
    "total_documents": 0,
    "documents_last_hour": 0,
    "documents_last_quarter": 0,
    "rate_per_minute": 0.0,
    "projected_next_hour": 0,
    "idle": True,
    "age_seconds": None,
    "buckets": [],
}


def fetch_growth(settings: Settings) -> dict[str, Any]:
    """The index's growth timeline, or a shaped stand-in when it cannot be read.

    Returned as data rather than raised for the same reason search is: this strip is
    on every page, and a monitoring panel that turns a page into an error report is
    worse than one that admits it has no reading yet.
    """
    status, payload = fetch_json("/analytics/growth", settings=settings)
    if status != 200 or not payload.get("available"):
        return dict(NO_GROWTH)

    merged = dict(NO_GROWTH)
    merged.update(payload)
    return merged


def smooth_path(vertices: list[tuple[float, float]]) -> str:
    """An SVG path through `vertices` that never leaves the range of its neighbours.

    Monotone cubic interpolation (Fritsch-Carlson), not an ordinary spline, and the
    difference is the whole reason this is written out: a plain cubic through a spiky
    series -- and a per-minute count of commits is nothing else -- overshoots, so it
    would draw growth in a minute where nothing arrived and dips below zero where the
    index was simply idle. This construction is limited by each segment's own slope,
    so the curve stays inside the box its data defines. Smoothing a reading is fine;
    inventing one is not.
    """
    if not vertices:
        return ""
    if len(vertices) == 1:
        x, y = vertices[0]
        return f"M {x:.3f} {y:.3f}"

    xs = [point[0] for point in vertices]
    ys = [point[1] for point in vertices]
    count = len(vertices)

    widths = [xs[index + 1] - xs[index] for index in range(count - 1)]
    slopes = [
        (ys[index + 1] - ys[index]) / widths[index] if widths[index] else 0.0
        for index in range(count - 1)
    ]

    # Start from the average of the two neighbouring slopes, then limit each tangent
    # so it cannot exceed three times the segment slope beside it. That bound is what
    # keeps a spike from becoming a wave.
    tangents = [0.0] * count
    tangents[0] = slopes[0]
    tangents[-1] = slopes[-1]
    for index in range(1, count - 1):
        if slopes[index - 1] * slopes[index] <= 0:
            tangents[index] = 0.0
        else:
            tangents[index] = (slopes[index - 1] + slopes[index]) / 2

    for index in range(count - 1):
        if slopes[index] == 0.0:
            tangents[index] = tangents[index + 1] = 0.0
            continue
        left = tangents[index] / slopes[index]
        right = tangents[index + 1] / slopes[index]
        radius = math.hypot(left, right)
        if radius > 3.0:
            scale = 3.0 / radius
            tangents[index] = scale * left * slopes[index]
            tangents[index + 1] = scale * right * slopes[index]

    parts = [f"M {xs[0]:.3f} {ys[0]:.3f}"]
    for index in range(count - 1):
        third = widths[index] / 3
        parts.append(
            f"C {xs[index] + third:.3f} {ys[index] + tangents[index] * third:.3f} "
            f"{xs[index + 1] - third:.3f} "
            f"{ys[index + 1] - tangents[index + 1] * third:.3f} "
            f"{xs[index + 1]:.3f} {ys[index + 1]:.3f}"
        )
    return " ".join(parts)


def chart_paths(
    points: list[dict[str, Any]], *, peak: int, rate_per_minute: float
) -> dict[str, Any]:
    """The chart's SVG geometry: the curve, its fill, and the current-rate reference.

    Computed here rather than in the browser so the curve has exactly one
    implementation. What the browser does with these strings is interpolate the
    numbers in one into the numbers in the next (see search.js), so the live chart
    moves smoothly without a second copy of the maths to keep in step -- and the
    numbers are interpolable because both strings always come from here, with the same
    commands and the same count of them.

    The rate is drawn as a horizontal line rather than as a sloped tail, and that was
    a correction rather than a preference. A tail at the same per-minute scale as the
    curve is almost always off the top of the box: the rate is an average over fifteen
    minutes while the peak is one minute, so a 4.4/min average against a 12-document
    spike climbs 88 user units in a single minute and leaves the plot within two. A
    stroke pinned to the ceiling says "high" and nothing else. The horizontal line
    stays inside the box by construction (an average cannot exceed the window's maximum
    unless the maximum fell outside the window) and it says something a tail cannot:
    whether the minutes just gone were above or below the average that is being
    projected forward.
    """
    top = CHART_PAD
    bottom = CHART_HEIGHT - CHART_PAD
    band = bottom - top
    step = CHART_WIDTH / max(len(points) - 1, 1)

    vertices = [
        (
            round(index * step, 3),
            # Against the window's peak rather than a fixed maximum: a slow hour would
            # otherwise be a flat line on the axis, which is the reading the chart
            # exists to distinguish from a stopped one.
            round(bottom - (point["percent"] / 100) * band, 3),
        )
        for index, point in enumerate(points)
    ]

    line = smooth_path(vertices)
    first_x, _ = vertices[0]
    last_x, last_y = vertices[-1]
    area = f"{line} L {last_x:.3f} {CHART_HEIGHT:.3f} L {first_x:.3f} {CHART_HEIGHT:.3f} Z"

    # The reported rate against the window's own peak, which is the same scale the
    # curve uses. Clamped only for safety: a rate above the peak means the busiest
    # minute is older than the rate window, and the line then sits at the ceiling
    # rather than above it.
    rate_percent = min(100.0, rate_per_minute / peak * 100) if peak > 0 else 0.0
    rate_y = round(bottom - rate_percent / 100 * band, 3)

    return {
        "line": line,
        "area": area,
        # A zero-length path with a round cap, which is how a dot is drawn in a plot
        # whose two axes are scaled differently: a `<circle>` would come out an ellipse.
        "dot": f"M {last_x:.3f} {last_y:.3f} L {last_x:.3f} {last_y:.3f}",
        "rate_line": f"M 0.000 {rate_y:.3f} L {CHART_WIDTH:.3f} {rate_y:.3f}",
        "rate_percent": round(rate_percent, 2),
        # Carried with the geometry so the browser can label the line it just drew
        # without a second lookup that could be a reading out of date.
        "rate_per_minute": rate_per_minute,
    }


def growth_series(growth: dict[str, Any], *, now: float | None = None) -> dict[str, Any]:
    """The last hour as a dense per-minute series, oldest first.

    Dense on purpose. Drawing only the minutes that saw a commit would make a
    stopped indexer look like an unbroken plateau, because its quiet minutes would
    simply not be drawn -- the chart would show the shape of the data it has instead
    of the hours that passed. Every minute in the window is emitted, including the
    empty ones.

    The reference minute is the reader's own clock, not the newest bucket: that is
    what makes a stall visible, as trailing empty minutes rather than a chart that
    always ends at full activity.

    The SVG geometry is built at the end of this function (see :func:`chart_paths`),
    so the shape of the chart and the numbers under it are derived from one read of
    the timeline and cannot drift apart.
    """
    buckets = growth.get("buckets") or []
    by_minute = {
        int(entry["at"]): int(entry["documents"])
        for entry in buckets
        if isinstance(entry, dict) and "at" in entry and "documents" in entry
    }

    # The projection tail is drawn from the *reported* rate rather than one derived
    # from these buckets, so the tail and the "expected next hour" card cannot
    # disagree: both come from the same 15-minute average.
    rate_per_minute = float(growth.get("rate_per_minute") or 0.0)

    reference = int(now if now is not None else time.time()) // 60 * 60
    minutes = [reference - (CHART_MINUTES - 1 - index) * 60 for index in range(CHART_MINUTES)]
    values = [by_minute.get(minute, 0) for minute in minutes]
    peak = max(values) if values else 0

    points = [
        {
            "at": minute,
            "documents": value,
            # Scaled for the bar heights, so the template only has to multiply by
            # a percentage. Against the peak rather than against a fixed maximum:
            # a slow hour would otherwise be a flat line at the bottom.
            "percent": round(value * 100 / peak, 2) if peak else 0.0,
        }
        for minute, value in zip(minutes, values, strict=True)
    ]

    return {
        "points": points,
        "peak": peak,
        "window_minutes": CHART_MINUTES,
        **chart_paths(points, peak=peak, rate_per_minute=rate_per_minute),
    }


def elapsed_phrase(seconds: float) -> str:
    """A duration as the shortest phrase that is still unambiguous."""
    if seconds < 60:
        return f"{int(seconds)} s"
    if seconds < 3_600:
        return f"{int(seconds // 60)} min"
    return f"{seconds / 3_600:.1f} h"


def growth_state(growth: dict[str, Any]) -> str:
    """One phrase describing the indexer, for the always-visible strip.

    Two distinctions are carried here, and both are the difference between a panel
    that can be trusted and one that cannot:

    * "nothing happened" is not "nothing can happen" -- a zero next to a readable feed
      is quiet, a zero next to an unreadable one is a missing reading;
    * a rate is an *average over a window*, so it keeps describing a crawl that has
      already stopped. Reporting that in the present tense would be a claim the data
      does not support, so the phrasing follows the age of the last commit: live while
      one landed in the last minute, and "quiet since" after that, with the average
      labelled as an average.
    """
    if not growth.get("available"):
        return "no index reading yet"
    if growth.get("idle"):
        return "indexer idle"

    rate = float(growth.get("rate_per_minute") or 0.0)
    age = growth.get("age_seconds")
    averaged = f"{rate:,.0f}/min avg".replace(",", " ")

    if isinstance(age, (int, float)) and age > 60:
        return f"quiet {elapsed_phrase(float(age))} · {averaged}"
    if rate <= 0:
        return "indexer running"
    return f"indexing {rate:,.0f}/min".replace(",", " ")


def growth_is_live(growth: dict[str, Any]) -> bool:
    """Whether the strip should animate, which is a narrower claim than "not idle".

    The shimmer means "this is happening now". A crawl that stopped two minutes ago is
    neither idle nor happening, so it gets the quiet phrasing and no animation: the label
    is the one place on the page where motion is a claim about the present.
    """
    if not growth.get("available") or growth.get("idle"):
        return False
    age = growth.get("age_seconds")
    return not isinstance(age, (int, float)) or age <= 60


def positive_int(raw: str | None, default: int, *, minimum: int = 1) -> int:
    """Parses a whole number, falling back to `default` rather than raising.

    A hand-edited query string is a normal thing to receive; a 400 over it is not
    what a visitor wants, so an unparseable page number means page one.
    """
    if raw is None:
        return default
    try:
        value = int(raw)
    except ValueError:
        return default
    return value if value >= minimum else default


def create_app(settings: Settings | None = None) -> Flask:
    """Builds the search page application."""
    resolved = settings if settings is not None else get_settings()

    app = Flask(__name__)
    app.config["SETTINGS"] = resolved
    app.add_template_filter(safe_snippet, "snippet")
    app.add_template_filter(format_date, "date")
    app.add_template_filter(format_clock, "clock")
    app.add_template_filter(format_age, "age")
    app.add_template_filter(format_count, "count")

    def page_context(**extra: Any) -> dict[str, Any]:
        """Everything both pages need, plus whatever the route adds.

        The growth feed is read here rather than in each route so the strip cannot
        end up on one page and missing from another, and so a route only describes
        what makes it different.
        """
        growth = fetch_growth(resolved)
        return {
            "growth": growth,
            "growth_state": growth_state(growth),
            "growth_live": growth_is_live(growth),
            "growth_poll_ms": GROWTH_POLL_MS,
            "backend": resolved.backend_url,
            **extra,
        }

    @app.get("/")
    def index() -> str:
        """The landing page, or the results page when a query is given."""
        query = (request.args.get("q") or "").strip()
        page = positive_int(request.args.get("page"), 1)

        if not query:
            # The landing page states how much there is to search, so it asks for
            # the real document count rather than showing a zero.
            status, stats = fetch_json("/admin/index/stats", settings=resolved)
            return render_template(
                "search.html",
                **page_context(
                    query="",
                    results=[],
                    total=int(stats.get("doc_count") or 0) if status == 200 else 0,
                    page=1,
                    pages=0,
                    took_ms=None,
                    cached=False,
                    relaxed=False,
                    ranking=None,
                    cap_note=None,
                    reachable=0,
                    past_the_pool=False,
                    error=None,
                    suggestions=SUGGESTED_QUERIES,
                    page_size=DEFAULT_PAGE_SIZE,
                ),
            )

        limit = DEFAULT_PAGE_SIZE
        offset = (page - 1) * limit
        status, payload = fetch_json(
            "/search",
            params={"q": query, "limit": limit, "offset": offset},
            settings=resolved,
        )

        if status != 200:
            # Everything that can go wrong here is something the visitor can be told
            # about: no index yet, a rejected query, or an API that is not running.
            detail = payload.get("detail") or payload.get("error") or "search failed"
            logger.info("search failed", extra={"query": query, "status": status, "detail": detail})
            return (
                render_template(
                    "search.html",
                    **page_context(
                        query=query,
                        results=[],
                        total=0,
                        page=page,
                        pages=0,
                        took_ms=None,
                        cached=False,
                        relaxed=False,
                        ranking=None,
                        cap_note=None,
                        reachable=0,
                        past_the_pool=False,
                        error=str(detail),
                        suggestions=SUGGESTED_QUERIES,
                        page_size=limit,
                        # A rejected query is the caller's own input coming back at them,
                        # so the field takes the error state and shakes once. A backend
                        # that is down is not the visitor's mistake and does not.
                        shake=status == 400,
                        # And the heading follows the same distinction: telling someone
                        # that "search is not available" when search is working fine and
                        # their query was unparsable sends them to the wrong place.
                        invalid_query=status == 400,
                    ),
                ),
                200,
            )

        total = int(payload.get("total") or 0)
        reachable = int(payload.get("reachable") or 0)
        results = decorate(payload.get("results") or [])
        ranking = payload.get("ranking") or {}

        # The page count is the API's, not an estimate made here. `total` is how many
        # documents matched in the whole index -- 15 584 for `github` on this corpus --
        # while the reachable pool is what the ranking policy can hand out, because it
        # works on a bounded candidate window. A pager built from `total` offered 1 172
        # pages of which the second was empty. It then compensated by noticing an empty
        # page and stepping the count back, and finally by dividing the pool size by the
        # page size -- which is *also* wrong, because a page may hold fewer than `limit`
        # results and the division undercounts (eight on the first page, four on the rest,
        # for `github`). The walk knows how many pages it produced; it reports that.
        pages = int(payload.get("pages") or 0)
        if not pages:
            pages = 1 if total else 0
        held_back = int(ranking.get("capped_on_page") or 0)

        # Hand-typed page numbers beyond the reachable pool: the pager never offers them,
        # but a URL can. "Nothing matched" would be a lie about this case -- something did
        # match, there is simply no page there -- so it gets its own state.
        past_the_pool = page > pages

        # A page shorter than it asked for, with candidates held back by the cap, is the
        # one shape where the per-site cap is the reason and saying so is useful: every
        # remaining candidate is from a site already on the page. That is the single-site
        # corpus case, and the fix is a raised cap.
        short_by_cap = bool(results) and len(results) < limit and held_back > 0

        return render_template(
            "search.html",
            **page_context(
                query=query,
                results=results,
                total=total,
                page=page,
                pages=pages,
                took_ms=payload.get("took_ms"),
                cached=bool(payload.get("cached")),
                relaxed=bool(payload.get("relaxed")),
                ranking=ranking,
                # Told plainly rather than shown as "nothing matched", because something
                # did match: the cap is simply why the rest of it is not on this page.
                cap_note=(ranking.get("max_per_domain") if short_by_cap else None),
                # What paging can reach, and whether this request went past it. The other
                # honest empty state: the pool ran out, so paging stops here even though
                # the index holds more matches than that.
                reachable=reachable,
                past_the_pool=past_the_pool,
                error=None,
                suggestions=SUGGESTED_QUERIES,
                page_size=limit,
            ),
        )

    @app.get("/stats")
    def stats() -> str:
        """The growth page: how fast the index is filling, and how fast that implies.

        A page rather than a dashboard panel because the question it answers -- "is
        this thing still finding pages?" -- is asked about the index, and the index
        has no other view. The numbers are all derived from the indexer's own
        timeline (see ``api.services.index_state.read_index_history``), so the page
        is correct for a server that started a second ago.
        """
        growth = fetch_growth(resolved)
        series = growth_series(growth)

        # The newest minutes, newest first, empty ones included: a table that
        # silently skipped the quiet minutes would make a stalled indexer look busy.
        recent = list(reversed(series["points"]))[:12]

        return render_template(
            "stats.html",
            **page_context(
                series=series,
                recent=recent,
                chart_minutes=CHART_MINUTES,
            ),
        )

    @app.get("/api/growth")
    def api_growth() -> tuple[Response, int]:
        """The growth feed as JSON, for the live panels, with the series already derived.

        Proxied rather than fetched from the API directly by the browser: the API is
        bound to loopback and is not a browser-facing service, so going through here
        keeps one origin, one shape and one place where an outage is described.

        The dense series is computed here and not in the browser, and that is about
        correctness rather than taste: an earlier version derived it in JavaScript and
        keyed the minutes in milliseconds against a feed that counts in seconds, so every
        live poll redrew the chart as sixty empty bars. Two implementations of one series
        is one more than can be right -- this one is covered by tests, and the page just
        draws what it is handed.
        """
        status, payload = fetch_json("/analytics/growth", settings=resolved)
        if status != 200:
            return jsonify(payload), status

        enriched = dict(payload)
        enriched["series"] = growth_series(enriched)
        return jsonify(enriched), 200

    @app.get("/search")
    def search_redirect() -> Response:
        """Keeps `?q=` on the landing path canonical.

        A form posts to `/`, so anything arriving at `/search` is a hand-written
        URL or an old bookmark; sending it on rather than 404-ing costs one redirect
        and keeps one URL shape in the address bar.
        """
        query = (request.args.get("q") or "").strip()
        target = url_for("index", q=query) if query else url_for("index")
        return redirect(target, code=308)

    @app.get("/api/search")
    def api_search() -> tuple[Response, int]:
        """The same query as JSON, for anything that prefers it."""
        query = (request.args.get("q") or "").strip()
        if not query:
            return jsonify({"error": "missing query", "detail": "Pass ?q=."}), 422

        limit = positive_int(request.args.get("limit"), DEFAULT_PAGE_SIZE)
        offset = positive_int(request.args.get("offset"), 0, minimum=0)
        status, payload = fetch_json(
            "/search", params={"q": query, "limit": limit, "offset": offset}, settings=resolved
        )
        return jsonify(payload), status

    @app.get("/healthz")
    def healthz() -> tuple[Response, int]:
        """Describes this process and the API behind it, as two separate facts."""
        status, payload = fetch_json("/health", settings=resolved)
        return jsonify(
            {
                "search_page": "ok",
                "backend": resolved.backend_url,
                "backend_status": status,
                "backend_reachable": status == 200,
                "index_open": payload.get("index_open") if status == 200 else None,
                "detail": None if status == 200 else payload.get("detail") or payload.get("error"),
            }
        ), 200

    return app


app = create_app()

if __name__ == "__main__":
    logging.basicConfig(level=get_settings().log_level)
    outcome = play_startup_sound(get_settings())
    logger.info("startup sound", extra={"result": outcome})
    app.run(host=get_settings().host, port=get_settings().search_port, threaded=True)
