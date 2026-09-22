"""The dashboard service.

    uv run flask --app dashboard.app run --port 5000

A Flask app whose only job is to read the JSON API over HTTP and turn it into
something a person can look at. That is a different job from serving queries, so
it is a different process:

* the dashboard polls every two seconds, and that traffic should not share a
  worker with the query path it is measuring;
* when the API is down, the dashboard must still come up and *say* the API is
  down, which it cannot do if it shares the API's lifecycle;
* it needs no index handle, no mmap, no Tantivy — the whole Rust stack stays out
  of this process.

Every upstream call goes through :func:`fetch_json`, so there is exactly one
place that knows the timeout, the error shape and the "unreachable" state.
"""

from __future__ import annotations

import logging
from typing import Any

import httpx
from flask import Flask, Response, jsonify, render_template, request

from api.config import Settings, get_settings
from api.sound import play_startup_sound

logger = logging.getLogger(__name__)

# Sent with every upstream request so a dashboard refresh is identifiable in the
# API's own logs, next to the queries it triggered.
DASHBOARD_CLIENT = "mjlsearch-dashboard"


def _client(settings: Settings) -> httpx.Client:
    """A short-lived client, deliberately not pooled.

    A dashboard refresh is a handful of requests every two seconds; a connection
    pool would be held open between them for no measurable benefit, and would
    outlive the settings it was built from in tests.
    """
    return httpx.Client(
        base_url=settings.backend_url,
        timeout=settings.backend_timeout_seconds,
        headers={"x-request-id": f"{DASHBOARD_CLIENT}", "user-agent": DASHBOARD_CLIENT},
    )


def fetch_json(
    path: str,
    *,
    params: dict[str, Any] | None = None,
    method: str = "GET",
    settings: Settings | None = None,
) -> tuple[int, dict[str, Any]]:
    """Calls the API, returning ``(status, payload)`` and never raising.

    A transport failure comes back as 502 with a payload that names the backend
    and the reason, because "the API is not answering" is exactly the state a
    dashboard exists to display. Letting it raise would turn the one page that
    could explain the outage into a 500 of its own.
    """
    resolved = settings if settings is not None else get_settings()

    try:
        with _client(resolved) as client:
            response = client.request(method, path, params=params)
    except httpx.HTTPError as error:
        logger.warning(
            "backend unreachable",
            extra={"backend": resolved.backend_url, "path": path, "detail": str(error)},
        )
        return 502, {
            "error": "backend unreachable",
            "backend": resolved.backend_url,
            "detail": str(error),
        }

    try:
        payload = response.json()
    except ValueError:
        return 502, {
            "error": "backend returned a non-JSON body",
            "backend": resolved.backend_url,
            "status": response.status_code,
        }

    if not isinstance(payload, dict):
        payload = {"data": payload}

    return response.status_code, payload


def create_app(settings: Settings | None = None) -> Flask:
    """Builds the dashboard application."""
    resolved = settings if settings is not None else get_settings()

    app = Flask(__name__)
    # The template embeds the poll interval and the backend address, so a stale
    # page cannot keep querying an address that has moved.
    app.config["SETTINGS"] = resolved

    @app.get("/")
    def index() -> str:
        """The single page. Everything else on it is fetched from the API."""
        return render_template(
            "dashboard.html",
            backend=resolved.backend_url,
            poll_ms=resolved.poll_ms,
            poll_seconds=resolved.poll_ms / 1000,
        )

    @app.get("/api/overview")
    def overview() -> tuple[Response, int]:
        """Everything the panels render, in one request.

        One proxy hop rather than six: the API already merges these into a single
        response precisely so a polling page does not multiply its own load.
        """
        status, payload = fetch_json("/analytics/overview", settings=resolved)
        return jsonify(payload), 200 if status == 200 else status

    @app.get("/api/search")
    def search() -> tuple[Response, int]:
        """Proxies a query, validating the parameters here as well.

        The API validates too. Doing it on both sides is not redundancy for its
        own sake: the dashboard should not send a request it already knows will
        be rejected, and the API should not trust a caller that has its own bugs.
        """
        query = (request.args.get("q") or "").strip()
        if not query:
            return jsonify(
                {"error": "missing query", "detail": "Pass ?q= with a search term."}
            ), 422

        params: dict[str, Any] = {"q": query}
        for name, default in (("limit", resolved.default_limit), ("offset", 0)):
            raw = request.args.get(name)
            try:
                params[name] = int(raw) if raw is not None else default
            except ValueError:
                return jsonify(
                    {
                        "error": "invalid parameter",
                        "detail": f"{name} must be a whole number, got {raw!r}.",
                    }
                ), 422

        status, payload = fetch_json("/search", params=params, settings=resolved)
        return jsonify(payload), status

    @app.post("/api/reset")
    def reset() -> tuple[Response, int]:
        """Clears the API's query log.

        The only mutating route here, and it touches nothing but the API's own
        memory. Reindexing is CLI-only and stays that way.
        """
        status, payload = fetch_json("/analytics/reset", method="POST", settings=resolved)
        if status == 204:
            payload = {"cleared": True}
        return jsonify(payload), 200 if status == 204 else status

    @app.get("/api/health")
    def health() -> tuple[Response, int]:
        """Describes this process *and* the API it reads.

        Two separate facts, kept separate: the dashboard being up says nothing
        about the API, and conflating them would make an API outage look like a
        dashboard outage.
        """
        status, payload = fetch_json("/health", settings=resolved)
        return jsonify(
            {
                "dashboard": "ok",
                "backend": resolved.backend_url,
                "backend_status": status,
                "backend_reachable": status == 200,
                "backend_health": payload if status == 200 else None,
                "detail": None if status == 200 else payload.get("detail") or payload.get("error"),
            }
        ), 200

    return app


app = create_app()

if __name__ == "__main__":
    # The development server. For anything longer-lived, run it under a real WSGI
    # server instead (gunicorn, waitress): see the README.
    logging.basicConfig(level=get_settings().log_level)
    outcome = play_startup_sound(get_settings())
    logger.info("startup sound", extra={"result": outcome})
    app.run(
        host=get_settings().host,
        port=get_settings().dashboard_port,
        threaded=True,
    )
