"""API endpoint tests."""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from api import dependencies
from api.config import get_settings
from api.main import create_app
from tests.fakes import VERSION, FakeExtension, hit


def test_health_reports_ok_and_the_extension_version(client: TestClient) -> None:
    response = client.get("/health")

    assert response.status_code == 200
    body = response.json()
    assert body["status"] == "ok"
    assert body["extension_version"] == VERSION
    assert body["index_open"] is True


def test_health_degrades_instead_of_failing_when_the_extension_is_missing(
    bare_client: TestClient,
) -> None:
    response = bare_client.get("/health")

    # Still a 200: a liveness probe that fails here would restart a container
    # that is working correctly and would hide the real cause.
    assert response.status_code == 200
    body = response.json()
    assert body["status"] == "degraded"
    assert "uv sync" in body["detail"]
    assert body["index_open"] is False


def test_health_degrades_when_the_index_could_not_be_opened(
    extension: FakeExtension, monkeypatch: pytest.MonkeyPatch, tmp_path
) -> None:
    extension.open_error = FileNotFoundError("no such directory")
    monkeypatch.setattr(dependencies, "get_search_core", lambda: extension)
    dependencies.reset_search_core()
    opened, reason = dependencies.open_index(tmp_path / "missing")

    assert opened is False
    assert "FileNotFoundError" in reason

    body = TestClient(create_app()).get("/health").json()

    assert body["status"] == "degraded"
    assert "FileNotFoundError" in body["detail"]
    assert body["index_open"] is False


def test_metrics_reads_resident_memory_from_this_process(client: TestClient) -> None:
    response = client.get("/metrics")

    assert response.status_code == 200
    body = response.json()
    assert body["process"]["rss_bytes"] > 0
    assert body["process"]["threads"] >= 1
    assert body["process"]["uptime_seconds"] > 0


def test_metrics_reports_the_size_that_gets_mapped(client: TestClient, env, tmp_path) -> None:
    # A directory with known contents, so the assertion is about the summation
    # rather than about whatever happens to be on this machine.
    index_dir = tmp_path / "index"
    (index_dir / "nested").mkdir(parents=True)
    (index_dir / "meta.json").write_bytes(b"x" * 100)
    (index_dir / "nested" / "segment.store").write_bytes(b"y" * 23)
    env(index_dir=index_dir)

    body = client.get("/metrics").json()

    assert body["index"]["doc_count"] == 42
    assert body["index"]["fingerprint"] == 7
    # Recursive: Tantivy puts segments in subdirectories, and a non-recursive
    # walk would under-report exactly the bytes that get mmap'd.
    assert body["index"]["index_bytes"] == 123
    assert body["index"]["path"] == str(index_dir)


def test_metrics_reports_zero_for_an_index_directory_that_does_not_exist(
    client: TestClient, env, tmp_path
) -> None:
    env(index_dir=tmp_path / "absent")

    # Zero rather than a fabricated size, and rather than an error: a fresh
    # checkout has no index directory at all.
    assert client.get("/metrics").json()["index"]["index_bytes"] == 0


def test_metrics_leaves_the_index_null_when_nothing_is_open(bare_client: TestClient) -> None:
    body = bare_client.get("/metrics").json()

    # Null rather than a fake zero: the two mean different things.
    assert body["index"] is None


def test_search_rejects_an_empty_query(client: TestClient) -> None:
    assert client.get("/search", params={"q": ""}).status_code == 422


def test_search_rejects_a_limit_above_the_configured_maximum(client: TestClient) -> None:
    response = client.get("/search", params={"q": "rust", "limit": 100_000})

    assert response.status_code == 422
    assert "limit" in response.json()["detail"]


def test_search_returns_the_extensions_hits_and_its_timing(client: TestClient) -> None:
    response = client.get("/search", params={"q": "rust"})

    assert response.status_code == 200
    body = response.json()
    assert body["query"] == "rust"
    assert body["total"] == 1
    assert body["limit"] == 10
    assert body["offset"] == 0
    assert body["cached"] is False
    assert body["relaxed"] is False
    assert body["took_ms"] > 0
    assert body["results"][0]["url"] == "https://example.com/a"
    assert body["results"][0]["snippet"].startswith("a snippet")


def test_the_page_is_a_window_into_the_ranked_pool(
    client: TestClient, extension: FakeExtension
) -> None:
    # Paging happens *after* the diversity cap, never in the engine. Asking the
    # engine for offset 2 would have it hand back a window that the cap then trims,
    # so a page could come back empty purely because the cap removed everything the
    # engine chose to put in it.
    # One host per result, so the diversity cap cannot interfere and the test is
    # about paging alone.
    extension.results = [
        hit(f"https://h{index}.example/{index}", f"page {index}") for index in range(5)
    ]
    extension.total_matches = 5

    body = client.get("/search", params={"q": "page", "limit": 2, "offset": 2}).json()

    assert extension.calls[-1]["offset"] == 0, "the engine must not page for us"
    assert extension.calls[-1]["limit"] > 2, "the engine must be asked for candidates"
    assert body["offset"] == 2
    assert [result["url"] for result in body["results"]] == [
        "https://h2.example/2",
        "https://h3.example/3",
    ]


def test_a_page_is_filled_even_when_one_host_owns_the_whole_pool(
    client: TestClient, extension: FakeExtension
) -> None:
    # The bug this pins: the cap was applied to a page fetched with exactly `limit`
    # candidates, so a corpus where every document shares one host -- a small or
    # single-site crawl, which is the common case -- returned two results for every
    # query, however many matched.
    extension.results = [
        hit(f"https://only.example/{index}", f"page {index}") for index in range(20)
    ]
    extension.total_matches = 1709

    body = client.get("/search", params={"q": "page", "limit": 10}).json()

    assert body["total"] == 1709
    # max_per_domain is 2, so a ten-wide page from one host can only ever show two
    # results -- and that is the cap working, not the page breaking.
    assert len(body["results"]) == 2
    # The point is that the engine was asked for more than the page needs, so the
    # cap is choosing from a real candidate set.
    assert extension.calls[-1]["limit"] > 10
    assert body["ranking"]["candidates"] > 10


def test_a_page_short_of_its_limit_is_re_asked_with_a_wider_window(
    client: TestClient, extension: FakeExtension
) -> None:
    # The measured case this pins: on a forty-site corpus the query `github` returned six
    # results for a ten-wide page while 1366 documents matched. The window happened to be
    # almost entirely github.com, and the two-per-site cap can only drop candidates it is
    # shown -- so six was the cap working on a window that had nothing else in it.
    #
    # The fix is to show it more, and to ask *after* seeing that the page is short rather
    # than guessing beforehand from the window's host distribution.
    per_host = get_settings().min_candidates
    extension.results = [
        hit(f"https://github.com/p{index}", f"GitHub {index}", score=9.0 - index / 1000)
        for index in range(per_host)
    ] + [hit(f"https://host{index}.example/a", f"Page {index}") for index in range(20)]
    extension.total_matches = 1366

    body = client.get("/search", params={"q": "github", "limit": 10}).json()

    assert len(extension.calls) == 2, "the first window is widened once"
    assert extension.calls[0]["limit"] == per_host
    assert extension.calls[-1]["limit"] == get_settings().max_candidates
    assert len(body["results"]) == 10, "the wider window is what fills the page"
    assert sum(r["url"].startswith("https://github.com") for r in body["results"]) == 2


def test_a_full_page_is_not_re_asked_for(client: TestClient, extension: FakeExtension) -> None:
    # The other half of the rule: widening costs a second engine call and a second cache
    # entry, so it is paid only when the page actually came up short.
    extension.results = [
        hit(f"https://host{index}.example/a", f"Page {index}") for index in range(30)
    ]
    extension.total_matches = 1366

    body = client.get("/search", params={"q": "anything", "limit": 10}).json()

    assert len(body["results"]) == 10
    assert len(extension.calls) == 1


def test_results_from_several_hosts_still_fill_a_full_page(
    client: TestClient, extension: FakeExtension
) -> None:
    # The half of the same bug that users actually notice: over-fetching is what
    # lets a full page be assembled from many hosts when the engine's top results
    # happen to be one host's.
    extension.results = [
        hit("https://one.example/a", "a", score=9.0),
        hit("https://one.example/b", "b", score=8.0),
        hit("https://one.example/c", "c", score=7.0),
        hit("https://two.example/a", "d", score=6.0),
        hit("https://three.example/a", "e", score=5.0),
        hit("https://four.example/a", "f", score=4.0),
    ]
    extension.total_matches = 6

    body = client.get("/search", params={"q": "anything", "limit": 4}).json()

    urls = [result["url"] for result in body["results"]]
    assert len(urls) == 4, f"the cap should not shorten the page: {urls}"
    assert sum(url.startswith("https://one.example") for url in urls) == 2


def test_search_forwards_the_request_id_so_the_rust_logs_correlate(
    client: TestClient, extension: FakeExtension
) -> None:
    client.get("/search", params={"q": "rust"}, headers={"x-request-id": "trace-me"})

    assert extension.calls[-1]["request_id"] == "trace-me"


def test_search_reports_a_rejected_query_as_a_client_error(
    client: TestClient, extension: FakeExtension
) -> None:
    extension.search_error = ValueError("unclosed quote")

    response = client.get("/search", params={"q": '"unclosed'})

    assert response.status_code == 400
    assert "unclosed quote" in response.json()["detail"]


def test_search_reports_a_runtime_failure_as_a_server_error(
    client: TestClient, extension: FakeExtension
) -> None:
    extension.search_error = RuntimeError("segment corrupt")

    response = client.get("/search", params={"q": "rust"})

    assert response.status_code == 500
    assert "segment corrupt" in response.json()["detail"]


def test_search_reports_unavailable_when_the_extension_is_missing(bare_client: TestClient) -> None:
    assert bare_client.get("/search", params={"q": "rust"}).status_code == 503


def test_search_says_so_when_there_is_no_index_yet(closed_client: TestClient) -> None:
    response = closed_client.get("/search", params={"q": "rust"})

    # Distinct from a missing extension: the server is fine, there is simply
    # nothing indexed, and the message says what to do about it.
    assert response.status_code == 503
    assert "index is not open" in response.json()["detail"]


def test_a_navigational_query_gets_the_wider_window_that_reaches_the_front_page(
    client: TestClient, extension
) -> None:
    # The measurement this pins: `github.com/` sits 358th of 642 in the engine's order for
    # the query `github`, because a front page is long marketing text in which the site's
    # name is diluted, while its documentation index is short and repeats it. No floor
    # worth paying reaches rank 358, so a navigational query is re-asked with the widest
    # window -- and only a navigational one, which is what keeps the engine's LRU entries
    # for ordinary queries small.
    filler = [
        hit(f"https://github.com/page{index}", f"GitHub page {index}", score=20.0 - index / 100)
        for index in range(200)
    ]
    extension.results = [
        hit("https://docs.github.com/en", "GitHub Docs", score=9.3),
        *filler,
        hit("https://github.com/", "GitHub", score=3.3),
    ]
    extension.total_matches = len(extension.results)

    body = client.get("/search", params={"q": "github", "limit": 3}).json()

    assert [entry["url"] for entry in body["results"]][0] == "https://github.com/"
    assert extension.calls[-1]["limit"] == get_settings().max_candidates, "the second ask is wide"
    assert extension.calls[0]["limit"] < extension.calls[-1]["limit"], "and the first is not"


def test_an_ordinary_query_is_asked_for_exactly_once(client: TestClient, extension) -> None:
    # The widening is paid for by navigational queries only; everything else keeps one
    # engine call and the smaller cache entry that goes with it.
    extension.results = [
        hit("https://a.example/one", "One", score=9.0),
        hit("https://b.example/two", "Two", score=8.0),
    ]
    extension.total_matches = 2

    client.get("/search", params={"q": "borrowing rust", "limit": 3})

    assert len(extension.calls) == 1


def test_search_applies_freshness_and_domain_diversity(
    client: TestClient, extension: FakeExtension
) -> None:
    # Three results from one host with the same age, plus one from another host:
    # the cap must bite and the freshness boost must reorder.
    extension.results = [
        hit("https://old.example/a", "old", score=5.0, fetched_at=1_000_000_000),
        hit("https://old.example/b", "older", score=4.0, fetched_at=1_000_000_000),
        hit("https://old.example/c", "oldest", score=3.0, fetched_at=1_000_000_000),
        hit("https://fresh.example/a", "fresh", score=1.0, fetched_at=2_000_000_000),
    ]
    extension.total_matches = 4

    body = client.get("/search", params={"q": "anything"}).json()

    assert body["ranking"]["max_per_domain"] == 2
    # The third old.example page is held back from *this page*, not dropped from the
    # pool: `suppressed` counts duplicates only, and the reachable count still includes
    # the candidate the cap set aside.
    assert body["ranking"]["capped_on_page"] == 1
    assert body["ranking"]["suppressed"] == 0
    assert body["reachable"] == 4
    urls = [result["url"] for result in body["results"]]
    assert len(urls) == 3
    # The fresh document now outranks the two old ones it was behind.
    assert urls[0] == "https://fresh.example/a"
    assert sum(url.startswith("https://old.example") for url in urls) == 2
    # `total` stays the engine's count: the ranking policy only shapes the page.
    assert body["total"] == 4


def test_a_capped_result_is_still_reachable_on_a_later_page(
    client: TestClient, extension: FakeExtension
) -> None:
    # The bug this pins, measured on the real corpus: the per-site cap was applied to the
    # whole candidate pool, so the query `github` -- 13 816 matches from four hosts in the
    # window -- left a pool of eight results, and page two came back empty while the page
    # offered a pager of 1 172 pages. Capping *per page* is what makes the walk work: the
    # skipped candidate is still there for the next page.
    extension.results = [hit(f"https://one.example/{index}", f"p{index}") for index in range(6)]
    extension.results += [hit(f"https://two.example/{index}", f"q{index}") for index in range(6)]
    extension.total_matches = 12

    first = client.get("/search", params={"q": "page", "limit": 4}).json()
    second = client.get("/search", params={"q": "page", "limit": 4, "offset": 4}).json()

    assert len(first["results"]) == 4
    assert len(second["results"]) == 4, "page two is filled, not empty"
    assert second["reachable"] == 12
    # Walked, not divided: the pool holds twelve and a page holds four, so three pages.
    assert second["pages"] == 3
    # Still no more than two from either host on one page, which is what the cap is for.
    for body in (first, second):
        hosts = [result["url"].split("/")[2] for result in body["results"]]
        assert hosts.count("one.example") <= 2
        assert hosts.count("two.example") <= 2
    # And the two pages do not repeat a result.
    seen = {result["url"] for result in first["results"]}
    assert not seen & {result["url"] for result in second["results"]}


def test_search_can_be_run_without_post_processing(
    client: TestClient, extension: FakeExtension, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Patched on the cached settings object rather than through the environment:
    # the settings are cached process-wide by design, so changing the environment
    # mid-test would have no effect on an instance another fixture already built.
    monkeypatch.setattr(get_settings(), "ranking_enabled", False)
    extension.results = [
        hit("https://a.example/1", "first", score=5.0),
        hit("https://a.example/2", "second", score=4.0),
        hit("https://a.example/3", "third", score=3.0),
    ]
    extension.total_matches = 3

    body = client.get("/search", params={"q": "anything"}).json()

    assert body["ranking"] is None
    assert len(body["results"]) == 3


def test_index_stats_are_read_only_and_real(client: TestClient) -> None:
    response = client.get("/admin/index/stats")

    assert response.status_code == 200
    body = response.json()
    assert body["doc_count"] == 42
    assert body["fingerprint"] == 7
    assert body["cache_entries"] == 3


def test_there_is_no_endpoint_that_can_reindex(client: TestClient) -> None:
    # Reindexing is CLI-only. This asserts the absence, so that adding one later
    # is a deliberate change to this test rather than a silent one.
    for path in ("/admin/reindex", "/admin/index", "/reindex"):
        assert client.post(path).status_code in (404, 405)


def test_index_stats_report_unavailable_without_an_index(bare_client: TestClient) -> None:
    assert bare_client.get("/admin/index/stats").status_code == 503


def test_request_id_from_the_caller_is_accepted(client: TestClient) -> None:
    response = client.get("/health", headers={"x-request-id": "trace-me"})

    # The header is consumed by the correlation dependency and must not break
    # the request; Rust receives the same value.
    assert response.status_code == 200
