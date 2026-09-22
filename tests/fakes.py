"""A stand-in for the compiled extension.

Lives outside ``conftest.py`` so the tests can import the same helper the fixtures
build on, rather than reaching into pytest's plugin module.

The fake is deliberately *not* a MagicMock: it answers with the exact dict shape
the PyO3 bindings produce, so the tests exercise the real conversion and ranking
code instead of a mock's idea of it.
"""

from __future__ import annotations

from typing import Any

VERSION = "9.9.9"


def hit(
    url: str,
    title: str,
    *,
    score: float = 1.0,
    fetched_at: int = 1_700_000_000,
    authority: float = 0.0,
    host_authority: float = 0.0,
) -> dict[str, Any]:
    """One result in the shape the extension returns it.

    The two authority figures are part of that shape now, so they belong here: a
    fake that omitted them would let the ranking policy read a default and pass
    while the real binding sent something else.
    """
    return {
        "url": url,
        "title": title,
        "snippet": f"a snippet mentioning <b>{title}</b>",
        "fetched_at": fetched_at,
        "score": score,
        "authority": authority,
        "host_authority": host_authority,
    }


class FakeExtension:
    """A stand-in for the ``search_core`` module."""

    def __init__(
        self,
        *,
        results: list[dict[str, Any]] | None = None,
        total_matches: int | None = None,
        cached: bool = False,
        relaxed: bool = False,
        took_ms: float = 1.5,
        doc_count: int = 42,
        search_error: Exception | None = None,
        open_error: Exception | None = None,
    ) -> None:
        self.results = results if results is not None else [hit("https://example.com/a", "A")]
        self.total_matches = total_matches if total_matches is not None else len(self.results)
        self.cached = cached
        self.relaxed = relaxed
        self.took_ms = took_ms
        self.doc_count = doc_count
        self.search_error = search_error
        self.open_error = open_error

        self.opened_paths: list[str] = []
        self.calls: list[dict[str, Any]] = []
        self.cache_entries_requested: int | None = None
        self.warmed: bool | None = None
        self._open = False

    def version(self) -> str:
        return VERSION

    def open_index(
        self,
        path: str,
        cache_entries: int | None = None,
        warm: bool = True,
    ) -> bool:
        """Opens the index, recording the cache and warm-up arguments.

        The signature matches the PyO3 binding, including the two tuning
        arguments, so a mismatch shows up here rather than as a `TypeError` at
        start-up.
        """
        if self.open_error is not None:
            raise self.open_error
        self.opened_paths.append(path)
        self.cache_entries_requested = cache_entries
        self.warmed = warm
        if self._open:
            return False
        self._open = True
        return True

    def is_open(self) -> bool:
        return self._open

    def search(
        self,
        query: str,
        limit: int = 10,
        offset: int = 0,
        request_id: str | None = None,
    ) -> dict[str, Any]:
        self.calls.append(
            {"query": query, "limit": limit, "offset": offset, "request_id": request_id}
        )
        if self.search_error is not None:
            raise self.search_error

        window = self.results[offset : offset + limit]
        return {
            "query": query,
            "total_matches": self.total_matches,
            "doc_count": self.doc_count,
            "took_ms": self.took_ms,
            "cached": self.cached,
            "relaxed": self.relaxed,
            "limit": limit,
            "offset": offset,
            "results": window,
        }

    def index_stats(self) -> dict[str, Any]:
        if not self._open:
            raise RuntimeError("the index is not open yet")
        return {"doc_count": self.doc_count, "fingerprint": 7, "cache_entries": 3}
