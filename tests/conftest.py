"""Shared fixtures.

The Rust extension is faked rather than imported, so this suite runs on a machine
where the wheel has not been built. ``tests/test_bridge.py`` is the file that
asserts the real extension works.
"""

from __future__ import annotations

from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest
from fastapi.testclient import TestClient

from api import dependencies
from api.config import get_settings
from api.main import create_app
from tests.fakes import FakeExtension


@pytest.fixture
def env(monkeypatch: pytest.MonkeyPatch) -> Callable[..., None]:
    """Sets ``SEARCH_*`` variables *and* drops the cached settings.

    Both halves are needed and forgetting either is a silent test bug: the
    settings object is cached for the process's lifetime, and the fixtures that
    open an index have usually already built one, so setting the environment
    alone leaves the old values in place. That is exactly how a test ends up
    reading the developer's real ``data/`` directory.
    """

    def apply(**values: Any) -> None:
        for key, value in values.items():
            monkeypatch.setenv(f"SEARCH_{key.upper()}", str(value))
        get_settings.cache_clear()

    return apply


@pytest.fixture
def extension() -> FakeExtension:
    """A fake extension with one matching document."""
    return FakeExtension()


@pytest.fixture
def client(
    extension: FakeExtension, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> Iterator[TestClient]:
    """A client whose index is open, so the query path is fully exercised."""
    monkeypatch.setattr(dependencies, "get_search_core", lambda: extension)
    dependencies.reset_search_core()
    dependencies.query_log().clear()
    # Opened the same way start-up opens it, so the tests cover that path too
    # instead of poking the module-level flag directly.
    opened, reason = dependencies.open_index(tmp_path / "index")
    assert opened, reason

    yield TestClient(create_app())


@pytest.fixture
def closed_client(
    extension: FakeExtension, monkeypatch: pytest.MonkeyPatch
) -> Iterator[TestClient]:
    """A client with a working extension but no index opened."""
    monkeypatch.setattr(dependencies, "get_search_core", lambda: extension)
    dependencies.reset_search_core()
    dependencies.query_log().clear()

    yield TestClient(create_app())


@pytest.fixture
def bare_client(monkeypatch: pytest.MonkeyPatch) -> Iterator[TestClient]:
    """A client with no extension at all, i.e. a machine with no built wheel."""
    dependencies.reset_search_core()

    def explode() -> None:
        raise RuntimeError("the search_core extension is not available; run `uv sync`")

    monkeypatch.setattr(dependencies, "get_search_core", explode)
    yield TestClient(create_app())


@pytest.fixture(autouse=True)
def _isolated_settings() -> Iterator[None]:
    """Clears the cached settings and query log around every test.

    Settings are cached process-wide for good reason, but that makes them shared
    state between tests: an env change in one test would otherwise leak into the
    next.
    """
    get_settings.cache_clear()
    dependencies.reset_search_core()
    dependencies.query_log().clear()
    yield
    get_settings.cache_clear()
    dependencies.reset_search_core()
    dependencies.query_log().clear()
