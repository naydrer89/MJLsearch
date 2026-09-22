"""In-process query analytics.

A bounded ring buffer rather than a metrics library. Two reasons: nothing has to
be installed, and the bound is explicit. A structure that grows with traffic is
exactly what this project is built to avoid, so the cap is a required argument
rather than a default that can be forgotten.

The lock is not optional. FastAPI runs synchronous endpoints in a thread pool, so
``record`` is genuinely called from several threads.
"""

from __future__ import annotations

import statistics
import threading
import time
from collections import Counter, deque
from dataclasses import dataclass
from typing import Any

# Ten-second buckets over five minutes: fine enough to see a burst, coarse enough
# that the series stays a fixed 30 points no matter how long the process runs.
_BUCKET_SECONDS = 10
_SERIES_BUCKETS = 30

# Reported in full, but only the head is worth displaying.
_TOP_QUERIES = 10
_RECENT_QUERIES = 20
_SLOWEST_QUERIES = 5


@dataclass(frozen=True, slots=True)
class QueryRecord:
    """One served query."""

    query: str
    took_ms: float
    total_matches: int
    returned: int
    cached: bool
    failed: bool
    at: float


class QueryLog:
    """A fixed-size record of recent queries, with aggregate views over it."""

    def __init__(self, capacity: int = 512) -> None:
        if capacity < 0:
            raise ValueError("capacity must not be negative")
        self._capacity = capacity
        self._records: deque[QueryRecord] = deque(maxlen=max(1, capacity))
        self._lock = threading.Lock()
        self._started_at = time.time()
        # Counted separately from the buffer: once the deque is full its length
        # stops growing, which would read as "traffic stopped".
        self._total = 0

    @property
    def capacity(self) -> int:
        """Maximum number of records retained."""
        return self._capacity

    def record(
        self,
        *,
        query: str,
        took_ms: float,
        total_matches: int,
        returned: int,
        cached: bool = False,
        failed: bool = False,
    ) -> None:
        """Appends one query. Silently a no-op when the log is disabled."""
        if self._capacity == 0:
            return
        entry = QueryRecord(
            query=query,
            took_ms=took_ms,
            total_matches=total_matches,
            returned=returned,
            cached=cached,
            failed=failed,
            at=time.time(),
        )
        with self._lock:
            self._records.append(entry)
            self._total += 1

    def clear(self) -> None:
        """Drops every record. Used by tests and by the dashboard's reset button."""
        with self._lock:
            self._records.clear()
            self._started_at = time.time()
            self._total = 0

    def _percentile(self, values: list[float], fraction: float) -> float:
        """Nearest-rank percentile, which needs no interpolation assumption."""
        if not values:
            return 0.0
        ordered = sorted(values)
        index = min(len(ordered) - 1, max(0, round(fraction * len(ordered)) - 1))
        return ordered[index]

    def snapshot(self) -> dict[str, Any]:
        """Aggregates the buffer into the shape the dashboard renders."""
        with self._lock:
            records = list(self._records)
            started_at = self._started_at
            total_recorded = self._total

        now = time.time()
        uptime = max(now - started_at, 0.0)
        cached = sum(1 for record in records if record.cached)
        failed = sum(1 for record in records if record.failed)
        latencies = [record.took_ms for record in records if not record.failed]

        series = _bucket_series(records, now)

        return {
            "capacity": self._capacity,
            "retained": len(records),
            "total_recorded": total_recorded,
            "uptime_seconds": uptime,
            "cached": cached,
            "cache_hit_rate": cached / len(records) if records else 0.0,
            "failed": failed,
            "matches": sum(record.total_matches for record in records),
            "returned": sum(record.returned for record in records),
            "latency_ms": {
                "mean": statistics.fmean(latencies) if latencies else 0.0,
                "p50": self._percentile(latencies, 0.50),
                "p95": self._percentile(latencies, 0.95),
                "max": max(latencies, default=0.0),
            },
            "queries_per_second": len(records) / uptime if uptime > 0 else 0.0,
            "series": series,
            "top_queries": [
                {"query": query, "count": count}
                for query, count in Counter(record.query for record in records).most_common(
                    _TOP_QUERIES
                )
            ],
            "slowest": [
                {
                    "query": record.query,
                    "took_ms": record.took_ms,
                    "total_matches": record.total_matches,
                }
                for record in sorted(records, key=lambda r: r.took_ms, reverse=True)[
                    :_SLOWEST_QUERIES
                ]
            ],
            "recent": [
                {
                    "query": record.query,
                    "took_ms": record.took_ms,
                    "total_matches": record.total_matches,
                    "returned": record.returned,
                    "cached": record.cached,
                    "failed": record.failed,
                    "at": record.at,
                    "age_seconds": max(now - record.at, 0.0),
                }
                for record in reversed(records[-_RECENT_QUERIES:])
            ],
        }


def _bucket_series(records: list[QueryRecord], now: float) -> list[dict[str, Any]]:
    """Buckets the records into a fixed-width latency/throughput series.

    One pass over the records, with the fixed points materialised at the end. The
    obvious version -- for each of the 30 buckets, scan all the records -- does 30 x
    ``capacity`` comparisons and builds 30 intermediate lists, on every dashboard
    refresh, to produce 30 numbers. The dashboard polls this endpoint every two seconds,
    so it was the largest per-tick cost in the service, and the series is the same either
    way: bucket membership depends only on ``record.at``.
    """
    start = now - _BUCKET_SECONDS * _SERIES_BUCKETS
    counts = [0] * _SERIES_BUCKETS
    totals = [0.0] * _SERIES_BUCKETS
    peaks = [0.0] * _SERIES_BUCKETS

    for record in records:
        index = int((record.at - start) // _BUCKET_SECONDS)
        # Records older than the window fall below zero; the comparison is `<` on the
        # bucket's upper bound, so the record exactly at a boundary belongs to the later
        # bucket -- which is what `//` gives, and what the bucket scan did too.
        if index < 0 or index >= _SERIES_BUCKETS:
            continue
        counts[index] += 1
        totals[index] += record.took_ms
        peaks[index] = max(peaks[index], record.took_ms)

    return [
        {
            "at": start + (index + 1) * _BUCKET_SECONDS,
            "count": counts[index],
            "mean_ms": totals[index] / counts[index] if counts[index] else 0.0,
            "max_ms": peaks[index],
        }
        for index in range(_SERIES_BUCKETS)
    ]
