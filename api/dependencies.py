"""Process-wide handles.

The Rust extension opens the Tantivy index once and mmaps it. That belongs at
process start, never per request, so this module is the only place that knows how
to obtain the module and when the index was opened.

The import is deliberately lazy: importing this package must not require the
compiled extension to exist, which is what lets the API tests run against a stub
and lets type checkers work on a machine with no Rust toolchain.
"""

from __future__ import annotations

import logging
import uuid
from pathlib import Path
from types import ModuleType

from fastapi import Request

from api.config import get_settings
from api.services.analytics import QueryLog

logger = logging.getLogger(__name__)

_search_core: ModuleType | None = None

# Whether the index handle was opened in this process, and why not when it was
# not. A missing index is a normal first-run state -- nothing has been crawled
# yet -- so it is reported rather than treated as a fatal condition.
_index_open = False
_index_open_error: str | None = None

_query_log = QueryLog(capacity=get_settings().query_log_size)

_BUILD_HINT = (
    "the search_core extension is not available; run `uv sync` from the repository "
    "root, or `maturin develop --release` inside crates/search-core"
)


def get_search_core() -> ModuleType:
    """Returns the extension module, raising if it cannot be loaded."""
    global _search_core
    if _search_core is None:
        try:
            import search_core
        except ImportError as exc:  # pragma: no cover - depends on build state
            raise RuntimeError(_BUILD_HINT) from exc
        _search_core = search_core
    return _search_core


def try_load_search_core() -> tuple[ModuleType | None, str | None]:
    """Returns the extension module, or ``(None, reason)`` if unavailable.

    Never raises. Health reporting needs to *describe* a missing extension, and a
    dependency that raises would turn that description into a 500.
    """
    try:
        return get_search_core(), None
    except RuntimeError as exc:  # pragma: no cover - depends on build state
        return None, str(exc)


def reset_search_core() -> None:
    """Drops the cached handles. Tests use this to swap in a stub."""
    global _search_core, _index_open, _index_open_error
    _search_core = None
    _index_open = False
    _index_open_error = None


def open_index(index_dir: Path | None = None) -> tuple[bool, str | None]:
    """Opens the index once, for the lifetime of the process.

    Never raises. A missing or empty index directory means the crawler and
    indexer have not produced anything yet, and the API must stay up and say so:
    the health endpoint reporting ``degraded`` with the reason is far more useful
    than a container that will not boot until a crawl has happened.
    """
    global _index_open, _index_open_error

    directory = index_dir if index_dir is not None else get_settings().index_dir

    module, reason = try_load_search_core()
    if module is None:
        _index_open_error = reason
        return False, reason

    try:
        # Idempotent on the Rust side, so a preloaded or re-entered lifespan
        # cannot end up with two mappings of the same index. The cache capacity is
        # passed in because it is a deployment decision. Opening also warms the
        # index, so the first real query does not pay for a cold mmap.
        settings = get_settings()
        module.open_index(
            str(directory),
            settings.query_cache_entries,
            settings.warm_index,
        )
    except Exception as error:  # noqa: BLE001 - any failure degrades, nothing aborts
        _index_open = False
        _index_open_error = f"{type(error).__name__}: {error}"
        return False, _index_open_error

    _index_open = True
    _index_open_error = None
    return True, None


def index_is_open() -> bool:
    """Whether this process holds an open, mmap'd index handle."""
    return _index_open


def index_open_error() -> str | None:
    """Why the index is not open, when it is not."""
    return _index_open_error


def query_log() -> QueryLog:
    """The process-wide query analytics buffer."""
    return _query_log


def get_request_id(request: Request) -> str:
    """Returns the caller's request ID, generating one when absent.

    Rust logs and Python logs share this value, which is what makes a single
    request traceable across the PyO3 boundary.
    """
    return request.headers.get("x-request-id") or uuid.uuid4().hex
