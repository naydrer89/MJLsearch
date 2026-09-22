"""Application entry point.

## Running it

    uvicorn api.main:app --workers 2          # the JSON API, port 8000
    uvicorn api.dashboard:app --port 5000     # the dashboard, one worker

## Why the worker count is bounded

Each worker is a separate process that opens the same index read-only and mmaps
it. The OS page cache shares those pages across processes, so a second worker does
not double the index's memory cost: it shares it. Past a small number of workers
the return flattens into CPU contention on the same pages, so two to four is the
useful range and more is not faster.

Workers must **spawn**, not fork a parent that already opened the index. Forking
after the mmap means every child inherits the same mappings and the same file
descriptors, and the reload semantics stop being well defined.

The dashboard is the exception: it runs one worker. Its query analytics live in
process memory, so a second worker would split the counters in two and the panels
would disagree with each other.
"""

from __future__ import annotations

import json
import logging
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from typing import Any

from fastapi import FastAPI
from fastapi.middleware.cors import CORSMiddleware

from api import dependencies
from api.config import get_settings
from api.routers import admin, analytics, health, search
from api.sound import play_startup_sound

logger = logging.getLogger(__name__)


class JsonFormatter(logging.Formatter):
    """Renders log records as one JSON object per line.

    Any key passed via ``extra=`` is included, which is how ``request_id`` ends up
    in the output and matches the field name used in the Rust ``tracing`` spans.
    """

    _STANDARD_ATTRS = frozenset(
        {
            "args",
            "asctime",
            "created",
            "exc_info",
            "exc_text",
            "filename",
            "funcName",
            "levelname",
            "levelno",
            "lineno",
            "message",
            "module",
            "msecs",
            "msg",
            "name",
            "pathname",
            "process",
            "processName",
            "relativeCreated",
            "stack_info",
            "taskName",
            "thread",
            "threadName",
        }
    )

    def format(self, record: logging.LogRecord) -> str:
        payload: dict[str, Any] = {
            "level": record.levelname,
            "logger": record.name,
            "message": record.getMessage(),
        }
        for key, value in record.__dict__.items():
            if key not in self._STANDARD_ATTRS and not key.startswith("_"):
                payload[key] = value
        if record.exc_info:
            payload["exception"] = self.formatException(record.exc_info)
        return json.dumps(payload, default=str)


def configure_logging(level: str) -> None:
    """Installs the JSON formatter on the root logger."""
    handler = logging.StreamHandler()
    handler.setFormatter(JsonFormatter())

    root = logging.getLogger()
    root.handlers.clear()
    root.addHandler(handler)
    root.setLevel(level.upper())


async def _startup() -> None:
    """Loads the extension, opens the index and announces the start.

    Ordered deliberately: the chime is last, so it means "the server is actually
    able to answer", not "a process was spawned".
    """
    settings = get_settings()
    configure_logging(settings.log_level)

    module, reason = dependencies.try_load_search_core()
    if module is None:
        logger.warning("search_core extension unavailable", extra={"detail": reason})
    else:
        logger.info(
            "search_core extension loaded",
            extra={"version": module.version()},
        )

    opened, index_reason = dependencies.open_index(settings.index_dir)
    if opened:
        logger.info("index opened", extra={"index_dir": str(settings.index_dir)})
    else:
        # Not fatal, and deliberately not raised: a fresh checkout has no index
        # yet, and the server must come up to say so.
        logger.warning("index not opened", extra={"detail": index_reason})

    result = play_startup_sound(settings)
    logger.info("startup sound", extra={"result": result})


@asynccontextmanager
async def lifespan(app: FastAPI) -> AsyncIterator[None]:
    """Runs the shared start-up sequence once per process."""
    await _startup()
    yield


def create_app(
    *,
    title: str = "MJLsearch",
    summary: str = "Rust-backed search engine with a Python API gateway.",
) -> FastAPI:
    """Builds the JSON API.

    JSON only. The dashboard is a separate Flask service (see ``dashboard/``) that
    reads this API over HTTP, which keeps this process free of templates, static
    files and a viewer's failure modes — and lets the dashboard report an API
    outage instead of going down with it.
    """
    app = FastAPI(title=title, version="0.1.0", summary=summary, lifespan=lifespan)

    settings = get_settings()
    if settings.cors_origins:
        app.add_middleware(
            CORSMiddleware,
            allow_origins=settings.cors_origins,
            allow_credentials=True,
            allow_methods=["GET", "POST"],
            allow_headers=["*"],
        )

    app.include_router(health.router)
    app.include_router(search.router)
    app.include_router(admin.router)
    app.include_router(analytics.router)

    return app


app = create_app()
