"""Query analytics: the bounded log and the aggregate endpoint."""

from __future__ import annotations

import time

import pytest
from fastapi.testclient import TestClient

from api import dependencies
from api.main import create_app
from api.services.analytics import QueryLog, QueryRecord


def test_the_log_evicts_old_records_instead_of_growing() -> None:
    log = QueryLog(capacity=3)

    for index in range(10):
        log.record(query=f"q{index}", took_ms=1.0, total_matches=0, returned=0)

    snapshot = log.snapshot()
    assert snapshot["retained"] == 3
    # The total keeps counting past the cap, otherwise a busy server would look
    # like it had stopped receiving queries the moment the buffer filled.
    assert snapshot["total_recorded"] == 10
    assert [entry["query"] for entry in snapshot["recent"]] == ["q9", "q8", "q7"]


def test_a_capacity_of_zero_retains_nothing() -> None:
    log = QueryLog(capacity=0)

    log.record(query="q", took_ms=1.0, total_matches=0, returned=0)

    snapshot = log.snapshot()
    assert snapshot["retained"] == 0
    assert snapshot["total_recorded"] == 0


def test_latency_percentiles_need_no_interpolation_assumptions() -> None:
    log = QueryLog(capacity=100)
    for value in range(1, 101):
        log.record(query="q", took_ms=float(value), total_matches=0, returned=0)

    latency = log.snapshot()["latency_ms"]
    assert latency["p50"] == 50.0
    assert latency["p95"] == 95.0
    assert latency["max"] == 100.0


def test_cache_hits_and_failures_are_counted_separately() -> None:
    log = QueryLog(capacity=100)
    log.record(query="a", took_ms=1.0, total_matches=1, returned=1, cached=True)
    log.record(query="a", took_ms=1.0, total_matches=1, returned=1, cached=True)
    log.record(query="b", took_ms=1.0, total_matches=0, returned=0, failed=True)

    snapshot = log.snapshot()
    assert snapshot["cached"] == 2
    assert snapshot["failed"] == 1
    assert snapshot["cache_hit_rate"] == pytest.approx(2 / 3)
    # A failed query has no meaningful latency and must not drag the percentiles.
    assert snapshot["latency_ms"]["max"] == 1.0


def test_the_series_is_a_fixed_size_whatever_the_traffic() -> None:
    log = QueryLog(capacity=1000)
    for _ in range(50):
        log.record(query="q", took_ms=2.0, total_matches=1, returned=1)

    series = log.snapshot()["series"]

    # Fixed width regardless of how long the process has run: a series that grew
    # with uptime would be a slow leak in a panel that polls forever.
    assert len(series) == 30
    assert sum(point["count"] for point in series) == 50


def test_records_fall_out_of_the_window_from_the_front() -> None:
    log = QueryLog(capacity=10)
    log.record(query="q", took_ms=1.0, total_matches=0, returned=0)

    # Reach into the buffer to age the single record beyond the five-minute
    # window; waiting for it in real time is not a test, it is a delay.
    log._records[0] = QueryRecord(  # noqa: SLF001 - deliberate
        query="q",
        took_ms=1.0,
        total_matches=0,
        returned=0,
        cached=False,
        failed=False,
        at=time.time() - 3_600,
    )

    series = log.snapshot()["series"]
    assert len(series) == 30
    assert sum(point["count"] for point in series) == 0
    assert log.snapshot()["retained"] == 1


def test_an_empty_log_reports_zeros_rather_than_failing() -> None:
    snapshot = QueryLog(capacity=10).snapshot()

    assert snapshot["total_recorded"] == 0
    assert snapshot["latency_ms"]["p95"] == 0.0
    assert snapshot["queries_per_second"] == 0.0
    assert snapshot["top_queries"] == []
    assert snapshot["recent"] == []


def test_the_endpoint_reports_a_served_query(client: TestClient) -> None:
    client.get("/search", params={"q": "rust"})

    body = client.get("/analytics/overview").json()

    assert body["queries"]["total_recorded"] == 1
    assert body["queries"]["top_queries"][0]["query"] == "rust"
    assert body["queries"]["recent"][0]["query"] == "rust"
    assert body["index"]["doc_count"] == 42
    assert body["process"]["rss_bytes"] > 0


def test_a_failed_query_still_reaches_the_analytics(client: TestClient, extension) -> None:
    extension.search_error = ValueError("bad syntax")

    client.get("/search", params={"q": "unbalanced("})

    body = client.get("/analytics/overview").json()
    assert body["queries"]["failed"] == 1
    assert body["queries"]["recent"][0]["failed"] is True


def test_the_crawler_and_spool_panels_are_null_before_anything_is_crawled(
    client: TestClient, env, tmp_path
) -> None:
    env(crawl_stats=tmp_path / "absent.json", spool_dir=tmp_path / "absent-spool")

    body = client.get("/analytics/overview").json()

    assert body["crawler"] is None
    assert body["spool"]["sealed_segments"] == 0
    assert body["spool"]["open_segments"] == 0


def test_the_crawler_panel_renders_a_published_stats_file(env, tmp_path) -> None:
    import json

    stats_path = tmp_path / "crawl-stats.json"
    stats_path.write_text(
        json.dumps(
            {
                "queued": 120,
                "fetched": 100,
                "indexed": 90,
                "failed": 4,
                "disallowed": 6,
                "frontier_pending": 20,
                "hosts": 3,
                "updated_at": int(time.time()),
            }
        ),
        encoding="utf-8",
    )
    env(crawl_stats=stats_path)

    body = TestClient(create_app()).get("/analytics/overview").json()

    assert body["crawler"]["indexed"] == 90
    assert body["crawler"]["frontier_pending"] == 20
    assert body["crawler"]["stale"] is False


def test_a_stale_stats_file_is_flagged_rather_than_hidden(env, tmp_path) -> None:
    import json

    stats_path = tmp_path / "crawl-stats.json"
    stats_path.write_text(
        json.dumps({"indexed": 5, "updated_at": int(time.time()) - 600}), encoding="utf-8"
    )
    env(crawl_stats=stats_path)

    body = TestClient(create_app()).get("/analytics/overview").json()

    assert body["crawler"]["stale"] is True
    assert body["crawler"]["age_seconds"] > 500


def test_a_half_written_stats_file_does_not_break_the_endpoint(env, tmp_path) -> None:
    stats_path = tmp_path / "crawl-stats.json"
    stats_path.write_text('{"queued": 12, "index', encoding="utf-8")
    env(crawl_stats=stats_path)

    body = TestClient(create_app()).get("/analytics/overview").json()

    assert body["crawler"] is None


def test_unknown_fields_in_the_stats_file_are_ignored(env, tmp_path) -> None:
    import json

    stats_path = tmp_path / "crawl-stats.json"
    stats_path.write_text(
        json.dumps({"indexed": 5, "updated_at": int(time.time()), "future_metric": 1}),
        encoding="utf-8",
    )
    env(crawl_stats=stats_path)

    # A field added by a newer crawler must not 500 the dashboard.
    body = TestClient(create_app()).get("/analytics/overview").json()
    assert body["crawler"]["indexed"] == 5


def test_the_spool_backlog_counts_sealed_and_open_segments_separately(env, tmp_path) -> None:
    spool = tmp_path / "spool"
    spool.mkdir()
    (spool / "segment-000000000001.pdoc").write_bytes(b"x" * 100)
    (spool / "segment-000000000002.pdoc").write_bytes(b"y" * 50)
    (spool / "segment-000000000003.open").write_bytes(b"z" * 10)
    env(spool_dir=spool)

    body = TestClient(create_app()).get("/analytics/overview").json()

    assert body["spool"]["sealed_segments"] == 2
    assert body["spool"]["sealed_bytes"] == 150
    assert body["spool"]["open_segments"] == 1


# ---------- the growth timeline ----------


def write_history(path, *, updated_at: int, total: int, buckets: list[tuple[int, int]]) -> None:
    import json

    path.write_text(
        json.dumps(
            {
                "updated_at": updated_at,
                "total_documents": total,
                "buckets": [{"at": at, "documents": documents} for at, documents in buckets],
            }
        ),
        encoding="utf-8",
    )


def test_the_growth_endpoint_derives_the_hour_from_the_timeline(env, tmp_path) -> None:
    now = int(time.time())
    minute = now // 60 * 60
    history = tmp_path / "index-history.json"
    write_history(
        history,
        updated_at=now,
        total=120,
        buckets=[
            # Twenty minutes ago: inside the hour, outside the rate window.
            (minute - 20 * 60, 70),
            # Three minutes ago: the one the rate is measured over.
            (minute - 3 * 60, 30),
            # Longer than an hour ago: retained by the indexer, not counted here.
            (minute - 100 * 60, 999),
        ],
    )
    env(index_history=history)

    body = TestClient(create_app()).get("/analytics/growth").json()

    assert body["available"] is True
    assert body["total_documents"] == 120
    assert body["documents_last_hour"] == 100
    assert body["documents_last_quarter"] == 30
    # 30 documents over fifteen minutes.
    assert body["rate_per_minute"] == 2.0
    assert body["projected_next_hour"] == 120
    assert body["idle"] is False


def test_an_indexer_that_stopped_is_reported_idle_rather_than_slow(env, tmp_path) -> None:
    history = tmp_path / "index-history.json"
    write_history(
        history, updated_at=int(time.time()) - 1_200, total=50, buckets=[(1_700_000_000, 50)]
    )
    env(index_history=history)

    body = TestClient(create_app()).get("/analytics/growth").json()

    # The rate is derived from the window, so an idle indexer reports zero even when
    # its last commit indexed a lot: nothing is arriving, which is the fact asked about.
    assert body["idle"] is True
    assert body["rate_per_minute"] == 0.0
    assert body["projected_next_hour"] == 0
    assert body["documents_last_hour"] == 0, "the buckets are old, not the reading"


def test_a_crawl_that_just_finished_is_not_reported_as_idle(env, tmp_path) -> None:
    # The bug this pins: the idle flag first used the crawler's ten-second staleness
    # threshold, so a corpus that had been drained two minutes ago was published as
    # "indexer idle" in the same response that extrapolated 5 456 documents from the
    # last fifteen minutes. Two contradictory claims, one page.
    now = int(time.time())
    minute = now // 60 * 60
    history = tmp_path / "index-history.json"
    write_history(
        history,
        updated_at=now - 120,
        total=1_252,
        buckets=[(minute - 2 * 60, 454), (minute - 60, 453), (minute, 457)],
    )
    env(index_history=history)

    body = TestClient(create_app()).get("/analytics/growth").json()

    assert body["idle"] is False
    assert body["rate_per_minute"] > 0
    assert body["projected_next_hour"] > 0, "a state and a projection must agree"


def test_the_growth_endpoint_answers_unavailable_before_a_first_commit(env, tmp_path) -> None:
    env(index_history=tmp_path / "absent.json")

    response = TestClient(create_app()).get("/analytics/growth")
    body = response.json()

    # Not a 500 and not a flat line: the panels say "nothing read yet" instead of
    # drawing zeroes that look like a stall.
    assert response.status_code == 200
    assert body["available"] is False
    assert body["documents_last_hour"] == 0


def test_a_half_written_timeline_does_not_break_the_growth_endpoint(env, tmp_path) -> None:
    history = tmp_path / "index-history.json"
    history.write_text('{"updated_at": 12, "buckets": [{"at":', encoding="utf-8")
    env(index_history=history)

    body = TestClient(create_app()).get("/analytics/growth").json()

    assert body["available"] is False


def test_resetting_clears_only_this_processes_memory(client: TestClient) -> None:
    client.get("/search", params={"q": "rust"})
    assert client.get("/analytics/overview").json()["queries"]["total_recorded"] == 1

    assert client.post("/analytics/reset").status_code == 204

    assert dependencies.query_log().snapshot()["total_recorded"] == 0


def test_the_series_matches_a_bucket_by_bucket_scan() -> None:
    """The one-pass series against the reference implementation it replaced.

    The rewrite is a performance change, so the test is an equivalence proof rather than a
    new expectation: same 30 points, same boundaries. The reference is the obvious version
    -- for each bucket, scan every record -- including the two edges that are easy to get
    wrong: a record exactly on a boundary belongs to the later bucket, and a record older
    than the window belongs to none.
    """
    from api.services.analytics import _BUCKET_SECONDS, _SERIES_BUCKETS, _bucket_series

    now = 1_800_000_000.0
    start = now - _BUCKET_SECONDS * _SERIES_BUCKETS
    offsets = [0.0, 5.0, _BUCKET_SECONDS, _BUCKET_SECONDS * 1.5, _BUCKET_SECONDS * 29 + 9.0]
    records = [
        QueryRecord(
            query=f"q{index}",
            took_ms=float(index),
            total_matches=1,
            returned=1,
            cached=False,
            failed=False,
            at=start + offset,
        )
        for index, offset in enumerate(offsets)
    ]
    # Nothing to bucket: older than the window, and "in the future" past its end.
    records.append(
        QueryRecord(
            query="old",
            took_ms=99.0,
            total_matches=0,
            returned=0,
            cached=False,
            failed=False,
            at=start - 1,
        )
    )
    records.append(
        QueryRecord(
            query="future",
            took_ms=99.0,
            total_matches=0,
            returned=0,
            cached=False,
            failed=False,
            at=now + 1,
        )
    )

    reference = []
    for index in range(_SERIES_BUCKETS):
        low = start + index * _BUCKET_SECONDS
        high = low + _BUCKET_SECONDS
        in_bucket = [record for record in records if low <= record.at < high]
        latencies = [record.took_ms for record in in_bucket]
        reference.append(
            {
                "at": high,
                "count": len(in_bucket),
                "mean_ms": sum(latencies) / len(latencies) if latencies else 0.0,
                "max_ms": max(latencies, default=0.0),
            }
        )

    assert _bucket_series(records, now) == pytest.approx(reference)
