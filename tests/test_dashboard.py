"""The Flask dashboard.

Two layers are tested, and they are tested separately on purpose:

* :func:`dashboard.app.fetch_json` — the one place that knows the timeout and the
  unreachable state, driven against a stubbed HTTP client;
* the routes — driven with ``fetch_json`` stubbed, so the assertions are about
  routing, validation and serialization rather than about HTTP plumbing.

Stubbing the client rather than the whole function for the first layer is what
keeps the 502-on-transport-failure behaviour covered: the code that classifies the
failure is the code under test.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import httpx
import pytest

from api.config import Settings
from dashboard import app as dashboard_app


class FakeResponse:
    """Stands in for ``httpx.Response``."""

    def __init__(self, status: int, payload: Any, *, text: bool = False) -> None:
        self.status_code = status
        self._payload = payload
        self._text = text

    def json(self) -> Any:
        if self._text:
            raise ValueError("not JSON")
        return self._payload


class FakeBackend:
    """Stands in for ``httpx.Client``, recording what was asked of it."""

    def __init__(
        self,
        responses: dict[str, FakeResponse] | None = None,
        error: Exception | None = None,
    ) -> None:
        self.responses = responses or {}
        self.error = error
        self.calls: list[dict[str, Any]] = []

    def __enter__(self) -> FakeBackend:
        return self

    def __exit__(self, *exc_info: object) -> bool:
        return False

    def request(self, method: str, path: str, params: dict[str, Any] | None = None) -> FakeResponse:
        self.calls.append({"method": method, "path": path, "params": params})
        if self.error is not None:
            raise self.error
        if path not in self.responses:
            raise AssertionError(f"the dashboard called an unexpected path: {path}")
        return self.responses[path]


@pytest.fixture
def settings() -> Settings:
    return Settings(backend_url="http://api.test:8000", backend_timeout_seconds=1.0, poll_ms=2000)


@pytest.fixture
def backend(monkeypatch: pytest.MonkeyPatch) -> FakeBackend:
    fake = FakeBackend()
    monkeypatch.setattr(dashboard_app, "_client", lambda settings: fake)
    return fake


@pytest.fixture
def dashboard(settings: Settings, monkeypatch: pytest.MonkeyPatch):
    """A Flask test client with the backend stubbed out entirely."""
    canned: dict[str, Any] = {"calls": []}

    def fake_fetch(path, *, params=None, method="GET", settings=None):  # noqa: ANN001
        canned["calls"].append({"path": path, "params": params, "method": method})
        return canned.get(path, (200, {"stub": True}))

    monkeypatch.setattr(dashboard_app, "fetch_json", fake_fetch)
    client = dashboard_app.create_app(settings).test_client()
    client.canned = canned  # type: ignore[attr-defined]
    return client


def overview_payload() -> dict[str, Any]:
    return {
        "generated_at": 1_700_000_000.0,
        "index": {
            "doc_count": 3,
            "index_bytes": 4123,
            "path": "data/index",
            "fingerprint": 7,
            "cache_entries": 0,
        },
        "crawler": None,
        "spool": {
            "sealed_segments": 0,
            "sealed_bytes": 0,
            "open_segments": 0,
            "path": "data/spool",
        },
        "queries": {},
        "process": {"rss_bytes": 1, "cpu_percent": 0.0, "threads": 2, "uptime_seconds": 1.0},
    }


# --------------------------------------------------------------------------- #
# the page
# --------------------------------------------------------------------------- #


def test_the_page_renders_with_the_backend_and_poll_interval(dashboard) -> None:
    response = dashboard.get("/")

    assert response.status_code == 200
    page = response.get_data(as_text=True)
    assert "http://api.test:8000" in page
    assert 'data-poll-ms="2000"' in page
    assert "Polling the API every 2.0s" in page


def test_the_search_input_has_a_real_label_and_a_form(dashboard) -> None:
    page = dashboard.get("/").get_data(as_text=True)

    # An input with only a placeholder has no accessible name.
    assert 'for="q"' in page
    assert 'name="q"' in page
    assert 'id="q"' in page
    assert "<form" in page
    assert 'autocomplete="off"' in page


def test_the_page_is_reachable_by_keyboard(dashboard) -> None:
    page = dashboard.get("/").get_data(as_text=True)

    assert "Skip to content" in page
    assert 'href="#main"' in page
    assert 'id="main"' in page


def test_async_regions_announce_themselves(dashboard) -> None:
    page = dashboard.get("/").get_data(as_text=True)

    # The notice banner and the result count are updated without a navigation, so
    # both need to be announced rather than silently repainted.
    assert page.count('aria-live="polite"') >= 2


def test_the_stylesheet_ships_reduced_motion_support(dashboard) -> None:
    response = dashboard.get("/static/style.css")

    assert response.status_code == 200
    assert "prefers-reduced-motion" in response.get_data(as_text=True)


def test_the_script_ships_and_is_served_from_the_dashboard(dashboard) -> None:
    response = dashboard.get("/static/dashboard.js")

    assert response.status_code == 200
    assert "/api/overview" in response.get_data(as_text=True)


def test_the_page_declares_a_light_colour_scheme(dashboard) -> None:
    page = dashboard.get("/").get_data(as_text=True)

    # Without this the browser paints dark form controls and scrollbars over a
    # light page.
    assert 'style="color-scheme: light"' in page
    assert '<meta name="theme-color" content="#f9f9f9">' in page


def test_fonts_are_preconnected_before_they_are_requested(dashboard) -> None:
    page = dashboard.get("/").get_data(as_text=True)

    assert page.index('rel="preconnect"') < page.index("fonts.googleapis.com/css2")


# --------------------------------------------------------------------------- #
# proxying
# --------------------------------------------------------------------------- #


def test_the_overview_is_proxied_in_one_request(dashboard) -> None:
    dashboard.canned["/analytics/overview"] = (200, overview_payload())

    response = dashboard.get("/api/overview")

    assert response.status_code == 200
    assert response.get_json()["index"]["doc_count"] == 3
    assert len(dashboard.canned["calls"]) == 1


def test_an_upstream_error_is_passed_through_rather_than_flattened(dashboard) -> None:
    dashboard.canned["/analytics/overview"] = (503, {"detail": "index is not open"})

    response = dashboard.get("/api/overview")

    # The status matters: a dashboard that turned every upstream failure into a
    # 200 with an empty panel would hide exactly what it exists to show.
    assert response.status_code == 503
    assert response.get_json()["detail"] == "index is not open"


def test_a_search_is_proxied_with_the_query_and_paging(dashboard) -> None:
    dashboard.canned["/search"] = (200, {"query": "rust", "total": 1, "results": []})

    response = dashboard.get("/api/search?q=rust&limit=5&offset=10")

    assert response.status_code == 200
    assert dashboard.canned["calls"][-1]["params"] == {"q": "rust", "limit": 5, "offset": 10}


def test_a_search_defaults_its_paging_from_the_settings(dashboard, settings: Settings) -> None:
    dashboard.canned["/search"] = (200, {"total": 0, "results": []})

    dashboard.get("/api/search?q=rust")

    assert dashboard.canned["calls"][-1]["params"]["limit"] == settings.default_limit
    assert dashboard.canned["calls"][-1]["params"]["offset"] == 0


def test_an_empty_query_is_refused_locally(dashboard) -> None:
    response = dashboard.get("/api/search?q=%20")

    assert response.status_code == 422
    assert "Pass ?q=" in response.get_json()["detail"]
    # Refused before it became a request: the API should not have to reject
    # something the caller already knew was invalid.
    assert dashboard.canned["calls"] == []


def test_a_non_numeric_limit_is_refused_with_the_offending_value(dashboard) -> None:
    response = dashboard.get("/api/search?q=rust&limit=soon")

    assert response.status_code == 422
    assert "soon" in response.get_json()["detail"]


def test_resetting_reports_success_for_a_204(dashboard) -> None:
    dashboard.canned["/analytics/reset"] = (204, {})

    response = dashboard.post("/api/reset")

    assert response.status_code == 200
    assert response.get_json() == {"cleared": True}
    assert dashboard.canned["calls"][-1]["method"] == "POST"


def test_health_separates_the_dashboard_from_the_api(dashboard) -> None:
    dashboard.canned["/health"] = (200, {"status": "ok", "extension_version": "0.1.0"})

    body = dashboard.get("/api/health").get_json()

    assert body["dashboard"] == "ok"
    assert body["backend"] == "http://api.test:8000"
    assert body["backend_reachable"] is True
    assert body["backend_health"]["extension_version"] == "0.1.0"


def test_health_stays_up_when_the_api_is_down(dashboard) -> None:
    dashboard.canned["/health"] = (502, {"error": "backend unreachable", "detail": "ConnectError"})

    response = dashboard.get("/api/health")
    body = response.get_json()

    # The dashboard answering about itself is the whole reason it is a separate
    # process; a 502 here would make the page that explains an outage useless.
    assert response.status_code == 200
    assert body["dashboard"] == "ok"
    assert body["backend_reachable"] is False
    assert "ConnectError" in body["detail"]


# --------------------------------------------------------------------------- #
# the transport layer
# --------------------------------------------------------------------------- #


def test_fetch_json_returns_the_payload_verbatim(settings: Settings, backend: FakeBackend) -> None:
    backend.responses["/search"] = FakeResponse(200, {"total": 2})

    status, payload = dashboard_app.fetch_json("/search", params={"q": "rust"}, settings=settings)

    assert status == 200
    assert payload == {"total": 2}
    assert backend.calls[0]["params"] == {"q": "rust"}


def test_a_transport_failure_becomes_a_named_unreachable_state(
    settings: Settings, backend: FakeBackend
) -> None:
    backend.error = httpx.ConnectError("connection refused")

    status, payload = dashboard_app.fetch_json("/analytics/overview", settings=settings)

    assert status == 502
    assert payload["error"] == "backend unreachable"
    assert payload["backend"] == "http://api.test:8000"
    assert "connection refused" in payload["detail"]


def test_a_timeout_is_reported_the_same_way_as_a_refusal(
    settings: Settings, backend: FakeBackend
) -> None:
    backend.error = httpx.ReadTimeout("timed out")

    status, payload = dashboard_app.fetch_json("/analytics/overview", settings=settings)

    # The operator needs the same answer either way: this API is not answering.
    assert status == 502
    assert "timed out" in payload["detail"]


def test_a_non_json_body_is_reported_rather_than_crashing(
    settings: Settings, backend: FakeBackend
) -> None:
    backend.responses["/health"] = FakeResponse(200, None, text=True)

    status, payload = dashboard_app.fetch_json("/health", settings=settings)

    assert status == 502
    assert "non-JSON" in payload["error"]


def test_the_client_is_built_with_the_configured_timeout(settings: Settings) -> None:
    client = dashboard_app._client(settings)

    assert client.timeout == httpx.Timeout(settings.backend_timeout_seconds)
    # The dashboard identifies itself upstream, so its refreshes are
    # distinguishable from a real client's queries in the API's logs.
    assert client.headers["user-agent"] == dashboard_app.DASHBOARD_CLIENT


def test_the_dashboard_never_opens_the_index_itself() -> None:
    # The whole point of the split: no Rust stack, no mmap, no Tantivy in the
    # process that only draws a page.
    source = Path(dashboard_app.__file__).read_text(encoding="utf-8")
    assert "import search_core" not in source
    assert "tantivy" not in source


def test_the_json_contract_between_the_two_services_is_kept() -> None:
    """The payload the API produces is the payload the dashboard consumes.

    Checked against the API's own response model rather than a hand-written
    fixture, so a field renamed in one place cannot silently break the other.
    """
    from api.models.responses import AnalyticsOverview

    payload = overview_payload()
    payload["queries"] = {
        "capacity": 512,
        "retained": 0,
        "total_recorded": 0,
        "uptime_seconds": 1.0,
        "cached": 0,
        "cache_hit_rate": 0.0,
        "failed": 0,
        "matches": 0,
        "returned": 0,
        "latency_ms": {"mean": 0.0, "p50": 0.0, "p95": 0.0, "max": 0.0},
        "queries_per_second": 0.0,
        "series": [{"at": 1.0, "count": 0, "mean_ms": 0.0, "max_ms": 0.0}],
        "top_queries": [],
        "slowest": [],
        "recent": [],
    }
    payload["index"]["index_bytes"] = 4123

    model = AnalyticsOverview.model_validate(payload)

    assert model.index is not None
    assert model.index.doc_count == 3


def test_the_removed_static_mount_is_really_gone() -> None:
    # The FastAPI app used to serve the page itself. Both dashboards existing at
    # once would be worse than either, so the absence is asserted.
    from api.main import create_app

    app = create_app()
    mounts = [route for route in app.routes if route.__class__.__name__ == "Mount"]

    assert mounts == []


def test_the_old_frontend_directory_is_not_referenced() -> None:
    root = Path(__file__).resolve().parent.parent
    assert not (root / "web").exists()
