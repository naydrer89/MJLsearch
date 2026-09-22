"""Read-only probes of the state other processes keep on disk.

Nothing here talks to the crawler or the indexer, and that is forced rather than
chosen: the crawler holds an exclusive lock on its RocksDB directory for as long
as it runs, so its progress has to be read from the small JSON file it publishes.
The same reasoning applies to the spool, which the two Rust processes pass
between them through the filesystem.

Every function is total: a missing file is a normal state (the crawler has not
started yet), not an error, and reports as ``None`` rather than raising. A
dashboard that 500s because the crawler is not running would be useless exactly
when it is most needed.
"""

from __future__ import annotations

import json
import logging
import os
import threading
import time
from pathlib import Path
from typing import Any

logger = logging.getLogger(__name__)

# Both files below are rewritten by other processes and read by more than one page on a
# timer, which is the whole reason these caches exist. The footer polls the growth feed
# every few seconds per open tab; without a cache every one of those polls re-reads and
# re-parses the indexer's whole timeline to derive numbers that cannot have changed.
#
# The key is the file's *identity* -- mtime in nanoseconds, size, inode -- not a clock.
# A fixed TTL would either be stale for a file that just changed or too short to help, and
# both writers publish atomically (write, rename), so a changed signature means a new file
# rather than a half-written one.
_TYPE_SIGNATURE = tuple[int, int, int]
_parse_cache: dict[Path, tuple[_TYPE_SIGNATURE, Any]] = {}
_parse_lock = threading.Lock()

# How long a summed directory size is reused. Long enough to stop the dashboard's two
# traces (metrics and overview) from walking the index tree every couple of seconds, short
# enough that a commit shows up while someone is watching the panel.
_DIRECTORY_SIZE_TTL_SECONDS = 5.0
_size_cache: dict[Path, tuple[float, int]] = {}
_size_lock = threading.Lock()


def _signature(path: Path) -> _TYPE_SIGNATURE | None:
    """The file's identity, or ``None`` when it cannot be stat'd."""
    try:
        stat = path.stat()
    except OSError:
        return None
    return (stat.st_mtime_ns, stat.st_size, stat.st_ino)


def _cached_json(path: Path) -> Any | None:
    """Parses ``path`` as JSON, reusing the previous parse while the file is unchanged.

    Returns ``None`` for a missing or unparsable file, which both callers treat as "no
    reading yet".

    The *same object* is returned on a hit, so callers must not mutate it: both of them
    build their own dict or list from it, and that is a requirement rather than a
    convention -- a caller annotating the cached dict would poison every later reader.
    """
    signature = _signature(path)
    if signature is None:
        return None

    with _parse_lock:
        cached = _parse_cache.get(path)
        if cached is not None and cached[0] == signature:
            return cached[1]

    try:
        raw = path.read_text(encoding="utf-8")
    except OSError as error:
        logger.debug("state file unreadable", extra={"path": str(path), "detail": str(error)})
        return None

    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        # Possible in principle despite the writers' atomic rename; "no reading yet" is
        # the right answer for a half-written file, and it will be re-read next poll.
        logger.debug("state file half-written", extra={"path": str(path)})
        return None

    with _parse_lock:
        _parse_cache[path] = (signature, parsed)
    return parsed


def clear_state_caches() -> None:
    """Drops both caches. For tests, and for a caller that just rewrote a file itself."""
    with _parse_lock:
        _parse_cache.clear()
    with _size_lock:
        _size_cache.clear()


# A stats file older than this is reported as stale. The crawler rewrites it
# every two seconds, so anything much older means it stopped.
STALE_AFTER_SECONDS = 10.0

# The window the "indexed in the last hour" panel asks about.
HOUR_SECONDS = 3_600
# The window the arrival rate is measured over. A quarter of an hour is short
# enough to react to a crawl that just started or just stopped, and long enough
# that one quiet minute does not read as a stopped indexer.
RATE_WINDOW_SECONDS = 900
# Minutes of history handed to the growth chart. One hour, matching the panel.
CHART_BUCKETS = 60
# How long without a commit means "this indexer has stopped".
#
# Deliberately the rate window rather than `STALE_AFTER_SECONDS`: a crawl that
# finished two minutes ago is not idle, it is a crawl that just finished, and
# reporting it as idle while the same response extrapolates a healthy rate from the
# last fifteen minutes would put two contradictory claims on one page. Past the rate
# window there is nothing left to extrapolate from, so the rate and the projection
# fall to zero together with the state.
IDLE_AFTER_SECONDS = RATE_WINDOW_SECONDS


def directory_bytes(path: Path, *, ttl: float = _DIRECTORY_SIZE_TTL_SECONDS) -> int:
    """Sums the file sizes under `path`, returning 0 when it does not exist.

    Deliberately not ``du``: spawning a process per dashboard refresh would be a fork
    storm for something a directory walk answers. The walk is bounded by the index's
    segment count, not by its document count.

    Two things make it cheap enough to sit behind a poll. The walk uses ``os.scandir``
    with an explicit stack rather than ``rglob``, which builds a ``Path`` per entry and
    materialises the entire tree before summing it; and the result is reused for a few
    seconds, because the dashboard asks for it every couple of seconds and a segment
    count does not change between two of those. A missing directory is not cached -- that
    answer is already free, and it is the one state that changes without a commit.
    """
    now = time.monotonic()
    with _size_lock:
        cached = _size_cache.get(path)
        if cached is not None and now - cached[0] < ttl:
            return cached[1]

    total = 0
    stack = [path]
    while stack:
        directory = stack.pop()
        try:
            with os.scandir(directory) as entries:
                for entry in entries:
                    try:
                        if entry.is_dir(follow_symlinks=False):
                            stack.append(entry.path)
                        elif entry.is_file(follow_symlinks=False):
                            total += entry.stat(follow_symlinks=False).st_size
                    except OSError:
                        continue
        except OSError:
            # Unreadable subdirectory, or the path itself is gone. Sum what was readable:
            # a partially readable index is still better described by most of its size
            # than by zero.
            continue

    if total == 0:
        return 0

    with _size_lock:
        _size_cache[path] = (now, total)
    return total


def read_index_stats(module: Any, index_dir: Path) -> dict[str, Any] | None:
    """Index statistics, combining the extension's view with the on-disk size.

    ``doc_count`` comes from the open handle because only Tantivy knows how many
    documents the committed segments hold. The byte count comes from the
    filesystem because it is the number that matters for the memory budget: it is
    what gets mmap'd.
    """
    try:
        stats = dict(module.index_stats())
    except Exception as error:  # noqa: BLE001 - reporting beats raising here
        logger.debug("index stats unavailable", extra={"detail": str(error)})
        return None

    stats["path"] = str(index_dir)
    stats["index_bytes"] = directory_bytes(index_dir)
    return stats


def read_crawl_stats(path: Path) -> dict[str, Any] | None:
    """The crawler's published progress, or ``None`` if it has never run."""
    raw = _cached_json(path)
    if not isinstance(raw, dict):
        return None

    # A copy per call, because the two fields below are relative to *now* and must not be
    # frozen into the cached parse: an age written into the cache would stop increasing,
    # and the panel would report a fresh crawler forever after it died.
    stats = dict(raw)
    updated_at = stats.get("updated_at")
    if isinstance(updated_at, (int, float)):
        stats["age_seconds"] = max(time.time() - updated_at, 0.0)
        stats["stale"] = stats["age_seconds"] > STALE_AFTER_SECONDS
    else:
        stats["age_seconds"] = None
        stats["stale"] = True

    return stats


def read_index_history(path: Path, *, now: float | None = None) -> dict[str, Any] | None:
    """The index's growth timeline, with the derived figures the panels show.

    Every count here is a count of documents the index *gained*, so the windows sum to
    no more than the total. That is a property of the timeline the indexer writes (see
    `crates/indexer/src/history.rs`), not of this function.

    Derivation happens here rather than in each page so that the footer and the
    stats page cannot disagree about how many documents arrived in the last hour —
    which they would, since the footer polls far more often than the page renders.

    ``idle`` is decided by the rate window rather than by the crawler's staleness
    threshold, so a state and a projection that come out of this function cannot
    contradict each other: past the window there is no rate to extrapolate, and the
    projection is zero for the same reason the state is idle.

    ``documents_last_hour`` is measured against the *timestamp in the file* rather
    than against the wall clock of a process that may have just started: the file
    is written by the indexer, so it, not the reader, knows when the last commit
    landed. A reader comparing the buckets against its own clock would report an
    empty hour for a full one whenever the two disagreed.
    """
    raw = _cached_json(path)
    if not isinstance(raw, dict):
        return None

    buckets: list[dict[str, int]] = []
    for entry in raw.get("buckets") or []:
        if not isinstance(entry, dict):
            continue
        at = entry.get("at")
        documents = entry.get("documents")
        if isinstance(at, (int, float)) and isinstance(documents, (int, float)):
            buckets.append({"at": int(at), "documents": int(documents)})
    buckets.sort(key=lambda bucket: bucket["at"])

    updated_at = raw.get("updated_at")
    updated_at = int(updated_at) if isinstance(updated_at, (int, float)) else None
    reference = updated_at if updated_at is not None else int(now or time.time())

    def documents_since(seconds: int) -> int:
        cutoff = reference - seconds
        return sum(bucket["documents"] for bucket in buckets if bucket["at"] >= cutoff)

    last_quarter = documents_since(RATE_WINDOW_SECONDS)
    rate_per_minute = last_quarter / (RATE_WINDOW_SECONDS / 60)

    age = None
    if updated_at is not None:
        age = max((now or time.time()) - updated_at, 0.0)

    return {
        "path": str(path),
        "updated_at": updated_at,
        "age_seconds": age,
        # Nothing has arrived for a while and nothing is expected until the
        # crawler and indexer run again. The panels say so rather than showing a
        # flat line with no explanation.
        "idle": age is not None and age > IDLE_AFTER_SECONDS,
        "total_documents": int(raw.get("total_documents") or 0),
        "documents_last_hour": documents_since(HOUR_SECONDS),
        "documents_last_quarter": last_quarter,
        "rate_per_minute": rate_per_minute,
        # A straight-line projection, and labelled as one: it is what the current
        # rate implies, not a prediction the system can make from one hour of data.
        "projected_next_hour": int(rate_per_minute * 60),
        "buckets": buckets[-CHART_BUCKETS:],
    }


def read_spool_backlog(spool_dir: Path) -> dict[str, Any]:
    """Counts sealed and in-progress spool segments.

    The two are reported separately because they mean different things: sealed
    segments are documents the indexer owes work on, while an open segment is
    just the crawler's current write buffer and is not a backlog at all.
    """
    backlog: dict[str, Any] = {
        "sealed_segments": 0,
        "sealed_bytes": 0,
        "open_segments": 0,
        "path": str(spool_dir),
    }

    try:
        entries = list(spool_dir.iterdir())
    except OSError:
        return backlog

    for entry in entries:
        try:
            if not entry.is_file():
                continue
            size = entry.stat().st_size
        except OSError:
            continue

        if entry.name.endswith(".pdoc"):
            backlog["sealed_segments"] += 1
            backlog["sealed_bytes"] += size
        elif entry.name.endswith(".open"):
            backlog["open_segments"] += 1

    return backlog
