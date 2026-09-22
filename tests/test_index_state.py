"""The read-only probes of other processes' files, and the caches in front of them.

Two kinds of assertion live here. The first is about the caches doing their job: a file
that has not changed must not be read or parsed again, because these reads sit behind
polled endpoints (the growth feed and the dashboard refresh) and the files are written by
other processes on their own schedule. The second is about the caches *not* doing damage:
the fields derived from the clock have to stay live on a cached parse, and a returned dict
has to be the caller's to mutate. A cache that got either of those wrong would be a
correctness bug wearing a performance win's clothes.
"""

from __future__ import annotations

import json
import time
from pathlib import Path

import pytest

from api.services import index_state


@pytest.fixture(autouse=True)
def clean_caches() -> None:
    """Both caches are module-level, so they outlive a test's ``tmp_path`` by design."""
    index_state.clear_state_caches()


def history_file(tmp_path: Path, *, updated_at: int, total: int = 5) -> Path:
    path = tmp_path / "index-history.json"
    path.write_text(
        json.dumps(
            {
                "updated_at": updated_at,
                "total_documents": total,
                "buckets": [{"at": updated_at - 60, "documents": 3}],
            }
        ),
        encoding="utf-8",
    )
    return path


def test_an_unchanged_timeline_is_parsed_once_for_many_reads(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The measured shape of the problem: the footer polls the growth feed every few
    # seconds per open tab, and each poll used to read and parse the indexer's whole
    # timeline to derive numbers that cannot have changed.
    path = history_file(tmp_path, updated_at=int(time.time()))
    parses = 0
    real_loads = json.loads

    def counting_loads(*args: object, **kwargs: object) -> object:
        nonlocal parses
        parses += 1
        return real_loads(*args, **kwargs)

    monkeypatch.setattr(index_state.json, "loads", counting_loads)

    for _ in range(20):
        assert index_state.read_index_history(path) is not None

    assert parses == 1, "twenty reads of one unchanged file is one parse"


def test_a_rewritten_timeline_is_read_again(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The other half of the contract, and the reason the cache key is the file's identity
    # rather than a clock: the indexer commits every couple of minutes and the page has to
    # see the commit. The size deliberately changes too, so this holds on a filesystem
    # with a coarse mtime.
    path = history_file(tmp_path, updated_at=1_000)
    parses = 0
    real_loads = json.loads

    def counting_loads(*args: object, **kwargs: object) -> object:
        nonlocal parses
        parses += 1
        return real_loads(*args, **kwargs)

    monkeypatch.setattr(index_state.json, "loads", counting_loads)

    first = index_state.read_index_history(path)
    assert first is not None
    assert first["total_documents"] == 5

    index_state.clear_state_caches()
    history_file(tmp_path, updated_at=2_000, total=9)

    second = index_state.read_index_history(path)
    assert second is not None
    assert second["total_documents"] == 9
    assert parses == 2


def test_the_clock_derived_fields_stay_live_on_a_cached_parse(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The trap this pins: caching the *returned* dict would freeze `age_seconds` and `idle`
    # at whatever they were on the first read, so a crawler that died would keep looking
    # fresh forever. Only the parse is cached; everything relative to now is computed per
    # call, and this asserts it by moving the clock with the file untouched.
    now = int(time.time())
    path = history_file(tmp_path, updated_at=now)

    monkeypatch.setattr(index_state.time, "time", lambda: now)
    first = index_state.read_crawl_stats(path)
    assert first is not None
    assert first["age_seconds"] == 0.0
    assert first["stale"] is False

    monkeypatch.setattr(index_state.time, "time", lambda: now + index_state.STALE_AFTER_SECONDS + 1)
    second = index_state.read_crawl_stats(path)

    assert second is not None
    assert second["age_seconds"] > index_state.STALE_AFTER_SECONDS
    assert second["stale"] is True


def test_calling_a_read_does_not_hand_out_a_handle_on_the_cache(tmp_path: Path) -> None:
    # Both readers build their own dict, because the alternative is a caller annotating a
    # shared object and every later reader inheriting the annotation. Asserted rather than
    # assumed: it is one `dict(...)` away from being wrong, silently.
    history = history_file(tmp_path, updated_at=int(time.time()))
    history_stats = index_state.read_index_history(history)
    assert history_stats is not None

    history_stats["total_documents"] = 999
    history_stats["buckets"].append({"at": 0, "documents": 0})

    again = index_state.read_index_history(history)
    assert again is not None
    assert again["total_documents"] == 5
    assert len(again["buckets"]) == 1


def test_the_directory_size_is_walked_once_per_window(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    index_dir = tmp_path / "shard-000"
    (index_dir / "nested").mkdir(parents=True)
    (index_dir / "meta.json").write_bytes(b"x" * 100)
    (index_dir / "nested" / "segment.store").write_bytes(b"y" * 23)

    walks = 0
    real_scandir = index_state.os.scandir

    def counting_scandir(path: object) -> object:
        nonlocal walks
        walks += 1
        return real_scandir(path)

    monkeypatch.setattr(index_state.os, "scandir", counting_scandir)

    assert index_state.directory_bytes(index_dir) == 123
    # Recursive, and reused: a second call inside the window answers from the cache, which
    # is what stops the dashboard's two traces from walking the segment tree every tick.
    assert index_state.directory_bytes(index_dir) == 123
    assert walks == 2, "one walk of the root plus one of the nested directory"

    # Past the window the walk happens again, so a commit shows up while someone watches.
    # The real clock is captured first: patching the module attribute and then calling
    # `time.monotonic` from inside the replacement would call the replacement.
    real_monotonic = time.monotonic
    monkeypatch.setattr(index_state.time, "monotonic", lambda: real_monotonic() + 100)
    (index_dir / "new.store").write_bytes(b"z" * 7)
    assert index_state.directory_bytes(index_dir) == 130


def test_a_missing_index_directory_is_reported_as_zero_and_not_cached(tmp_path: Path) -> None:
    # Zero rather than an error: a fresh checkout has no index at all. Not cached, because
    # that is the one state that changes without a commit, and the answer is already free.
    absent = tmp_path / "absent"
    assert index_state.directory_bytes(absent) == 0

    absent.mkdir()
    (absent / "meta.json").write_bytes(b"x" * 11)

    assert index_state.directory_bytes(absent) == 11
