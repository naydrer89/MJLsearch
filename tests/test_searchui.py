"""Tests for the search page.

The routes are driven with ``fetch_json`` stubbed, so the assertions are about what
the page renders rather than about HTTP. ``fetch_json`` itself is tested directly
against a fake backend, because it is the one place that decides what a failure
looks like.
"""

from __future__ import annotations

import re
from typing import Any

import httpx
import pytest
from flask.testing import FlaskClient

from api.config import Settings
from searchui import app as search_app


@pytest.fixture
def settings() -> Settings:
    """Settings pointed at a backend that will not answer unless a test stubs it."""
    return Settings(backend_url="http://127.0.0.1:9", backend_timeout_seconds=0.5)


@pytest.fixture
def client(settings: Settings, monkeypatch: pytest.MonkeyPatch) -> FlaskClient:
    """A client whose upstream calls are stubbed per test."""
    app = search_app.create_app(settings)
    app.config["TESTING"] = True
    return app.test_client()


# What a ``/analytics/growth`` response looks like with something to report.
GROWTH: dict[str, Any] = {
    "available": True,
    "path": "data/index-history.json",
    "updated_at": 1_700_000_000,
    "age_seconds": 2.0,
    "idle": False,
    "total_documents": 1_234,
    "documents_last_hour": 432,
    "documents_last_quarter": 108,
    "rate_per_minute": 7.2,
    "projected_next_hour": 432,
    "buckets": [{"at": 1_699_999_980, "documents": 12}],
}


def growth_payload(**extra: Any) -> dict[str, Any]:
    return {**GROWTH, **extra}


def stub(monkeypatch: pytest.MonkeyPatch, responses: dict[str, tuple[int, dict[str, Any]]]) -> None:
    """Answers ``fetch_json`` by path, so a test states only what it cares about.

    The growth feed has a default rather than being an error: the strip is on every
    page, so a test about a result list should not have to mention it. The default is
    the reading a fresh checkout has -- nothing drained yet -- which is also the state
    the panels are most likely to get wrong.
    """

    def fake(path: str, *, params: Any = None, settings: Any = None) -> tuple[int, dict[str, Any]]:
        if path in responses:
            return responses[path]
        if path == "/analytics/growth":
            return 200, {"available": False}
        raise AssertionError(f"unexpected upstream call to {path}")

    monkeypatch.setattr(search_app, "fetch_json", fake)


def hit(url: str, title: str, *, score: float = 1.0, snippet: str = "a snippet") -> dict[str, Any]:
    return {
        "url": url,
        "title": title,
        "snippet": snippet,
        "fetched_at": 1_700_000_000,
        "score": score,
    }


def results_payload(
    results: list[dict[str, Any]],
    total: int | None = None,
    reachable: int | None = None,
    pages: int | None = None,
    **extra: Any,
) -> dict[str, Any]:
    """A ``/search`` body, with the three counts a payload can carry.

    ``reachable`` defaults to the number of results in the payload rather than to
    ``total``, because that is the difference the pager depends on: ``total`` is what
    matched in the index and ``reachable`` is what paging can hand out. A stub that
    equated them would let a pager built from the wrong one pass every test. ``pages``
    is the API's own walked count, so it is stubbed in the same spirit -- derived here so
    a test that only cares about `total` still gets a coherent pair.
    """
    if reachable is None:
        reachable = len(results)
    return {
        "query": "q",
        "took_ms": 1.25,
        "total": total if total is not None else len(results),
        "reachable": reachable,
        "pages": pages if pages is not None else (-(-reachable // 10)),
        "limit": 10,
        "offset": 0,
        "cached": False,
        "relaxed": False,
        "results": results,
        "ranking": {
            "freshness_half_life_days": 30.0,
            "max_per_domain": 10,
            "suppressed": 0,
            "candidates": 10,
        },
        **extra,
    }


# ---------- the snippet escaping rule ----------


def test_entities_from_the_index_become_characters_not_text() -> None:
    # The bug this pins: the fragment arrives already escaped, so escaping it again
    # rendered every entity as its own literal text -- `documentation&quot;` on screen
    # instead of `documentation"`.
    rendered = str(search_app.safe_snippet("<b>Bloom</b> &quot;filter&quot;"))

    assert "&quot;" in rendered, "the quote must still be an entity in the markup"
    assert "&amp;quot;" not in rendered, "and must not be double-escaped"


def test_only_bold_tags_survive() -> None:
    # The safety property, stated as a test rather than trusted to a dependency:
    # anything else a crawled page got into its own text is shown as text.
    for hostile in (
        "&lt;script&gt;alert(1)&lt;/script&gt;",
        "<img src=x onerror=alert(1)>",
        "<a href='#'>x</a>",
        "&lt;b&gt; literal &lt;/b&gt;",
    ):
        rendered = str(search_app.safe_snippet(hostile))
        tags = [
            tag for tag in re.findall(r"</?([a-zA-Z][a-zA-Z0-9]*)", rendered) if tag.lower() != "b"
        ]
        assert not tags, f"{hostile!r} leaked {tags} into the markup"


def test_a_none_snippet_renders_nothing() -> None:
    assert str(search_app.safe_snippet(None)) == ""


# ---------- the landing page ----------


def test_the_landing_page_offers_the_queries_that_are_tested(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(monkeypatch, {"/admin/index/stats": (200, {"doc_count": 4321})})

    body = client.get("/").get_data(as_text=True)

    assert "Search 4" in body.replace(",", "")
    for suggestion in search_app.SUGGESTED_QUERIES:
        assert f">{suggestion}</a>" in body


def test_the_landing_page_survives_an_api_that_is_down(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A landing page that 500s because the API is restarting is worse than one that
    # loads and says it has nothing to search.
    stub(monkeypatch, {"/admin/index/stats": (502, {"error": "backend unreachable"})})

    response = client.get("/")

    assert response.status_code == 200
    assert "indexed documents" in response.get_data(as_text=True)


# ---------- results ----------


def test_a_query_lists_its_results_with_a_readable_url(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(
        monkeypatch,
        {
            "/search": (
                200,
                results_payload(
                    [hit("https://en.wikipedia.org/wiki/GitHub", "GitHub — Wikipedia")]
                ),
            )
        },
    )

    body = client.get("/?q=github").get_data(as_text=True)

    assert "GitHub — Wikipedia" in body
    # The breadcrumb, not the raw URL: what a results page shows.
    assert "en.wikipedia.org › wiki › GitHub" in body
    assert "https://en.wikipedia.org/wiki/GitHub" in body, "the link itself must be the real URL"


def test_a_widened_search_says_so_instead_of_looking_exact(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    # No page held every word, so the engine fell back to "any word". These results
    # are not what was asked for, and a page that presents them as an exact match is
    # telling the visitor their query was found when it was not.
    stub(
        monkeypatch,
        {
            "/search": (
                200,
                results_payload([hit("https://example.com/a", "A")], relaxed=True, total=7),
            )
        },
    )

    body = client.get("/?q=borrowing+garbage").get_data(as_text=True)

    assert "widened" in body
    assert "no page held every word" in body


def test_an_exact_search_carries_no_such_note(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The counterweight: the note above must mean something, so it cannot be always
    # on. A normal search shows the count and nothing about widening.
    stub(
        monkeypatch,
        {"/search": (200, results_payload([hit("https://example.com/a", "A")]))},
    )

    body = client.get("/?q=rust").get_data(as_text=True)

    assert "widened" not in body


def test_the_count_is_the_match_count_not_the_page_length(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(
        monkeypatch,
        {
            "/search": (
                200,
                results_payload(
                    [hit(f"https://a{index}.example/", f"page {index}") for index in range(3)],
                    total=812,
                ),
            )
        },
    )

    body = client.get("/?q=anything").get_data(as_text=True)

    assert "812" in body
    assert "results" in body


def test_paging_asks_the_engine_for_the_right_window_and_reports_it(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    seen: dict[str, Any] = {}

    def fake(path: str, *, params: Any = None, settings: Any = None) -> tuple[int, dict[str, Any]]:
        seen.update(params or {})
        return 200, results_payload([hit("https://a.example/1", "one")], total=95, reachable=95)

    monkeypatch.setattr(search_app, "fetch_json", fake)

    body = client.get("/?q=x&page=3").get_data(as_text=True)

    assert seen == {"q": "x", "limit": 10, "offset": 20}
    # The page count comes from `reachable`, so a stub that only sets `total` would
    # advertise a single page here -- which is the assertion's point.
    assert "Showing page 3 of 10" in body


def test_a_page_beyond_the_last_one_says_so_instead_of_erroring(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Page 99 of a pool that holds five results. Distinct from "nothing matched": five
    # documents did, there is simply no page 99 -- and the message has to say which of
    # the two it is, or it sends the visitor off to rewrite a query that works.
    stub(monkeypatch, {"/search": (200, results_payload([], total=5, reachable=5))})

    response = client.get("/?q=x&page=99")

    assert response.status_code == 200
    body = response.get_data(as_text=True)
    assert "No such page." in body
    assert "No document in the index matches that." not in body
    assert "Showing page 99" not in body


def test_a_page_the_per_site_cap_cannot_fill_is_not_advertised_before_it_is_reached(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The bug this pins: `total` counts matches in the whole index, `reachable` counts
    # what paging can hand out. This stub is the old trap verbatim -- eleven matches but
    # a pool that the cap leaves ten wide -- and the pager must not offer a second page
    # for it. (`reachable` is what it reads now instead of inferring it from `total`.)
    stub(
        monkeypatch,
        {
            "/search": (
                200,
                results_payload(
                    [],
                    total=11,
                    ranking={
                        "freshness_half_life_days": 30.0,
                        "max_per_domain": 10,
                        "suppressed": 1,
                        "candidates": 80,
                    },
                ),
            )
        },
    )

    body = client.get("/?q=amazon&page=2").get_data(as_text=True)

    # It says what happened instead of rendering an empty list under a "page 2 of 2"
    # heading, and it offers no third page.
    assert "No such page." in body
    assert "Showing page 2 of 2" not in body
    assert 'class="pager"' not in body, "no pager when there is nowhere to go"


def test_a_query_with_no_matches_names_the_query(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(monkeypatch, {"/search": (200, results_payload([], total=0))})

    body = client.get("/?q=zzzznotpresent").get_data(as_text=True)

    assert "No results for" in body
    assert "zzzznotpresent" in body


def test_the_cache_is_disclosed_because_it_changes_what_the_timing_means(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(monkeypatch, {"/search": (200, results_payload([], total=3, cached=True))})

    assert "from cache" in client.get("/?q=x").get_data(as_text=True)


def test_a_rejected_query_is_shown_as_a_message_not_a_500(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(monkeypatch, {"/search": (400, {"detail": "invalid query: unclosed quote"})})

    response = client.get("/?q=%22unclosed")
    body = response.get_data(as_text=True)

    assert response.status_code == 200
    assert "invalid query: unclosed quote" in body
    # The heading has to name the actual failure: telling someone search is unavailable
    # when search is working and their query was unparsable sends them to the wrong place,
    # and the repair on offer is a corrected query rather than a look at the crawler.
    assert "That query could not be parsed" in body
    assert "Search is not available" not in body
    assert "crawler and the indexer" not in body
    # The field takes the error state, which is also what triggers its shake.
    assert 'class="t-clear is-error"' in body


def test_an_index_that_was_never_built_explains_what_to_do(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(monkeypatch, {"/search": (503, {"detail": "index is not open: no index at data/index"})})

    body = client.get("/?q=x").get_data(as_text=True)

    assert "no index at data/index" in body
    assert "crawler and the indexer" in body


# ---------- the growth strip ----------


def test_the_strip_reports_what_arrived_and_that_the_indexer_is_running(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(
        monkeypatch,
        {
            "/admin/index/stats": (200, {"doc_count": 1_234}),
            "/analytics/growth": (200, growth_payload()),
        },
    )

    body = client.get("/").get_data(as_text=True)

    assert "in the last hour" in body
    # The figures themselves, formatted the way the page formats a result count.
    assert "1\u2009234" in body
    assert "432" in body
    assert "indexing 7" in body
    assert 'href="/stats"' in body, "the strip is how the growth page is reached"


def test_the_strip_calls_a_rate_an_average_once_the_crawl_has_stopped() -> None:
    # The claim this pins: `rate_per_minute` is an average over fifteen minutes, so it
    # keeps describing a crawl that has already stopped. Present tense about that is a
    # claim the data does not support, so the phrasing follows the last commit.
    assert (
        search_app.growth_state(
            {"available": True, "idle": False, "rate_per_minute": 91.0, "age_seconds": 2.0}
        )
        == "indexing 91/min"
    )

    quiet = search_app.growth_state(
        {"available": True, "idle": False, "rate_per_minute": 91.0, "age_seconds": 209.0}
    )
    assert quiet == "quiet 3 min · 91/min avg"
    assert "indexing" not in quiet

    assert search_app.growth_state({"available": True, "idle": True, "rate_per_minute": 0.0}) == (
        "indexer idle"
    )


def test_the_strip_says_it_has_no_reading_rather_than_showing_zeroes(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A zero next to a running indexer means quiet; a zero next to a feed that cannot
    # be read means the number is missing. The strip must not conflate them.
    stub(monkeypatch, {"/admin/index/stats": (502, {"error": "backend unreachable"})})

    body = client.get("/").get_data(as_text=True)

    assert "no index reading yet" in body
    assert "growth__pulse--idle" in body


def test_the_strip_is_on_a_results_page_too(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(
        monkeypatch,
        {
            "/search": (200, results_payload([hit("https://a.example/", "one")], total=1)),
            "/analytics/growth": (200, growth_payload(documents_last_hour=9)),
        },
    )

    body = client.get("/?q=x").get_data(as_text=True)

    assert "in the last hour" in body
    assert "indexing 7" in body


# ---------- the growth series ----------


def test_the_series_is_dense_so_a_gap_is_visible() -> None:
    # The shape of the bug this pins: drawing only the minutes that saw a commit makes
    # a stopped indexer look like an unbroken plateau, because its quiet minutes would
    # simply not be drawn.
    now = 1_700_000_000
    minute = now // 60 * 60
    series = search_app.growth_series(
        growth_payload(
            buckets=[
                {"at": minute - 120, "documents": 5},
                {"at": minute, "documents": 9},
            ]
        ),
        now=now,
    )

    points = {point["at"]: point for point in series["points"]}

    assert len(series["points"]) == search_app.CHART_MINUTES
    assert points[minute]["documents"] == 9
    assert points[minute - 120]["documents"] == 5
    # The minute between them saw nothing, and is drawn as a measured zero.
    assert points[minute - 60]["documents"] == 0
    # Scaled against the peak, so the busiest minute fills the chart.
    assert series["peak"] == 9
    assert points[minute]["percent"] == 100.0
    assert points[minute - 60]["percent"] == 0.0


def test_a_growth_page_with_no_history_reports_nothing_instead_of_a_flat_line() -> None:
    series = search_app.growth_series({"available": False, "buckets": []}, now=1_700_000_000)

    assert series["peak"] == 0
    assert {point["percent"] for point in series["points"]} == {0.0}


def test_the_window_ends_at_the_readers_minute_not_at_the_last_commit() -> None:
    # Otherwise a stalled indexer would always end its chart at full activity, which is
    # exactly the reading the chart exists to contradict.
    now = 1_700_000_000
    minute = now // 60 * 60
    series = search_app.growth_series(
        growth_payload(buckets=[{"at": minute - 40 * 60, "documents": 4}]), now=now
    )

    assert series["points"][-1]["at"] == minute
    assert series["points"][-1]["documents"] == 0, "the last drawn minute is this one"


# ---------- the chart's geometry ----------

# Every number in a path is either an x or a y -- `M x y`, then `C cx cy cx cy x y`
# repeated -- so the odd ones are all the y values, control points included.
Y_VALUES = re.compile(r"-?\d+(?:\.\d+)?")


def _chart() -> dict[str, Any]:
    now = 1_700_000_000
    minute = now // 60 * 60
    buckets = [
        {"at": minute - index * 60, "documents": (index * 7) % 13}
        for index in range(search_app.CHART_MINUTES)
    ]
    return search_app.growth_series(growth_payload(buckets=buckets), now=now)


def test_the_curve_never_leaves_the_range_its_data_defines() -> None:
    # The reason this is monotone interpolation and not an ordinary spline: a cubic
    # through a spiky series overshoots, and a per-minute count of commits is nothing
    # but spikes. An overshoot would draw growth in a minute where nothing arrived, so
    # the check is that no control point of any segment leaves the band the vertices
    # occupy.
    series = _chart()
    top, bottom = search_app.CHART_PAD, search_app.CHART_HEIGHT - search_app.CHART_PAD

    ys = [float(value) for value in Y_VALUES.findall(series["line"])][1::2]

    assert ys, "the line is drawn"
    assert min(ys) >= top, "no segment rises above the peak"
    assert max(ys) <= bottom, "no segment dips below the baseline"


def test_the_curve_still_rises_to_its_peak() -> None:
    # The other half of the limit: a curve that stayed inside the band by never rising
    # would be a straight line, which is a worse chart than an overshoot.
    series = _chart()
    top = search_app.CHART_PAD

    ys = [float(value) for value in Y_VALUES.findall(series["line"])][1::2]

    assert min(ys) == pytest.approx(top), "the busiest minute reaches the top of the band"


def test_the_rate_line_is_inside_the_plot_even_when_the_rate_exceeds_the_peak() -> None:
    # A rate above the window's peak is possible -- the peak minute may be older than
    # the fifteen-minute window the rate is averaged over -- and a reference line drawn
    # above the box would simply be invisible, which is a worse way of saying "high"
    # than one that runs along the ceiling.
    now = 1_700_000_000
    minute = now // 60 * 60
    series = search_app.growth_series(
        growth_payload(
            rate_per_minute=900.0,
            buckets=[{"at": minute, "documents": 3}],
        ),
        now=now,
    )
    top, bottom = search_app.CHART_PAD, search_app.CHART_HEIGHT - search_app.CHART_PAD

    rate_y = float(Y_VALUES.findall(series["rate_line"])[1])

    assert top <= rate_y <= bottom
    assert series["rate_percent"] == 100.0


def test_the_rate_line_sits_where_the_rate_says_it_should() -> None:
    # Scaled against the window's peak, like the curve, so the two can be compared on
    # sight: a rate of half the peak has to land halfway up the band.
    now = 1_700_000_000
    minute = now // 60 * 60
    series = search_app.growth_series(
        growth_payload(
            rate_per_minute=1.0,
            buckets=[{"at": minute - index * 60, "documents": 2} for index in range(3)],
        ),
        now=now,
    )
    top, bottom = search_app.CHART_PAD, search_app.CHART_HEIGHT - search_app.CHART_PAD
    rate_y = float(Y_VALUES.findall(series["rate_line"])[1])

    assert series["peak"] == 2
    assert rate_y == pytest.approx(bottom - 0.5 * (bottom - top))
    assert series["rate_per_minute"] == 1.0, "carried so the page can label the line it drew"


def test_the_dot_marks_the_newest_minute() -> None:
    # It is a zero-length path with a round cap rather than a `<circle>`: the plot's two
    # axes are scaled differently, and a circle drawn in that space comes out an ellipse.
    series = _chart()

    assert series["dot"].startswith("M 1000.000 "), "the newest minute is at the right edge"


# ---------- the growth page ----------


def test_the_growth_page_shows_the_projection_and_every_minute_of_the_window(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(monkeypatch, {"/analytics/growth": (200, growth_payload())})

    body = client.get("/stats").get_data(as_text=True)

    assert "Index growth" in body
    assert "Expected next hour" in body
    assert "432" in body
    # A projection, and labelled as one rather than presented as a fact.
    assert "projection, not a promise" in body

    # One line through every minute of the window, empty minutes included. The curve
    # carries one cubic segment per gap, which is how a quiet stretch is a drawn zero
    # rather than a missing point.
    line = re.search(r'data-line d="([^"]+)"', body)
    assert line, "the chart line is rendered"
    assert line.group(1).count("C") == search_app.CHART_MINUTES - 1

    # The rate the projection is extrapolated from, drawn as its own dashed path.
    assert re.search(r'data-line-rate d="M 0\.000 [\d.]+ L 1000\.000 [\d.]+"', body)
    assert 'data-growth-endpoint="/api/growth"' in body


def test_the_growth_page_explains_a_missing_reading_instead_of_drawing_one(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(monkeypatch, {"/analytics/growth": (200, {"available": False})})

    response = client.get("/stats")
    body = response.get_data(as_text=True)

    assert response.status_code == 200
    assert "No growth reading yet" in body


def test_the_growth_json_route_proxies_the_api_and_derives_the_series(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Proxied rather than called from the browser: the API is loopback-bound and not a
    # browser-facing service, so the page keeps one origin.
    now = 1_700_000_000
    minute = now // 60 * 60
    stub(
        monkeypatch,
        {
            "/analytics/growth": (
                200,
                growth_payload(buckets=[{"at": minute, "documents": 12}]),
            )
        },
    )
    monkeypatch.setattr(search_app.time, "time", lambda: float(now))

    payload = client.get("/api/growth").get_json()

    assert payload["documents_last_hour"] == 432
    assert payload["available"] is True

    # The series is derived server-side, because a second implementation in the browser
    # keyed its minutes in milliseconds against a feed that counts in seconds and redrew
    # a live chart as sixty empty bars. One implementation, covered here.
    series = payload["series"]
    assert len(series["points"]) == search_app.CHART_MINUTES
    assert series["points"][-1]["at"] == minute, "seconds, like the feed"
    assert series["points"][-1]["documents"] == 12
    assert series["peak"] == 12


# ---------- parameters and routes ----------


@pytest.mark.parametrize("value", ["0", "-4", "abc", "", "1.5"])
def test_a_nonsense_page_number_falls_back_to_the_first_page(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch, value: str
) -> None:
    seen: dict[str, Any] = {}

    def fake(path: str, *, params: Any = None, settings: Any = None) -> tuple[int, dict[str, Any]]:
        seen.update(params or {})
        return 200, results_payload([])

    monkeypatch.setattr(search_app, "fetch_json", fake)

    response = client.get("/?q=x&page=" + value)

    assert response.status_code == 200
    assert seen["offset"] == 0, "a hand-edited query string must not 400"


def test_the_search_path_moves_a_query_onto_the_canonical_one(client: FlaskClient) -> None:
    response = client.get("/search?q=rust")

    assert response.status_code == 308
    assert response.headers["Location"].endswith("/?q=rust")


def test_an_empty_query_lands_on_the_landing_page(client: FlaskClient) -> None:
    response = client.get("/search")

    assert response.status_code == 308
    assert response.headers["Location"].endswith("/")


def test_the_json_route_rejects_a_missing_query(client: FlaskClient) -> None:
    response = client.get("/api/search")

    assert response.status_code == 422
    assert "missing query" in response.get_json()["error"]


def test_healthz_separates_this_process_from_the_api(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(monkeypatch, {"/health": (200, {"status": "ok", "index_open": True})})

    payload = client.get("/healthz").get_json()

    assert payload["search_page"] == "ok"
    assert payload["backend_reachable"] is True
    assert payload["index_open"] is True


def test_healthz_still_answers_when_the_api_is_down(
    client: FlaskClient, monkeypatch: pytest.MonkeyPatch
) -> None:
    stub(monkeypatch, {"/health": (502, {"error": "backend unreachable", "detail": "refused"})})

    response = client.get("/healthz")

    assert response.status_code == 200
    assert response.get_json()["backend_reachable"] is False


# ---------- the upstream call ----------


class FakeBackend:
    """A transport that answers one canned response and records the request."""

    def __init__(self, status: int, body: Any) -> None:
        self.status = status
        self.body = body
        self.calls: list[tuple[str, dict[str, Any] | None]] = []

    def __enter__(self) -> FakeBackend:
        return self

    def __exit__(self, *_: Any) -> None:
        return None

    def get(self, path: str, params: dict[str, Any] | None = None) -> httpx.Response:
        self.calls.append((path, params))
        return httpx.Response(self.status, json=self.body, request=httpx.Request("GET", path))


def test_fetch_json_returns_the_payload_verbatim(
    settings: Settings, monkeypatch: pytest.MonkeyPatch
) -> None:
    backend = FakeBackend(200, {"total": 7, "results": []})
    monkeypatch.setattr(search_app.httpx, "Client", lambda **_: backend)

    status, payload = search_app.fetch_json("/search", params={"q": "x"}, settings=settings)

    assert status == 200
    assert payload == {"total": 7, "results": []}
    assert backend.calls == [("/search", {"q": "x"})]


def test_fetch_json_turns_a_transport_failure_into_a_state_the_page_can_show(
    settings: Settings, monkeypatch: pytest.MonkeyPatch
) -> None:
    def explode(**_: Any) -> Any:
        raise httpx.ConnectError("connection refused")

    monkeypatch.setattr(search_app.httpx, "Client", explode)

    status, payload = search_app.fetch_json("/search", settings=settings)

    assert status == 502
    assert payload["error"] == "backend unreachable"
    assert payload["backend"] == settings.backend_url


def test_fetch_json_reports_a_non_json_body_rather_than_raising(
    settings: Settings, monkeypatch: pytest.MonkeyPatch
) -> None:
    class NotJson:
        def __enter__(self) -> NotJson:
            return self

        def __exit__(self, *_: Any) -> None:
            return None

        def get(self, path: str, params: Any = None) -> httpx.Response:
            return httpx.Response(200, text="<html>", request=httpx.Request("GET", path))

    monkeypatch.setattr(search_app.httpx, "Client", lambda **_: NotJson())

    status, payload = search_app.fetch_json("/search", settings=settings)

    assert status == 502
    assert "non-JSON" in payload["error"]
