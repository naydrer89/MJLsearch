"""Liveness and resource metrics."""

from __future__ import annotations

import os
import time

import psutil
from fastapi import APIRouter

from api import dependencies
from api.config import get_settings
from api.models.responses import (
    HealthResponse,
    IndexStats,
    MetricsResponse,
    ProcessMetrics,
)
from api.services.index_state import read_index_stats

router = APIRouter(tags=["health"])

# Bound to this process, so the readings describe one uvicorn worker rather than
# the container as a whole. Summing across workers is the caller's job.
_process = psutil.Process(os.getpid())
_started_at = time.time()


def process_metrics() -> ProcessMetrics:
    """Resource readings for this process.

    Includes the mmap'd index pages the OS has faulted in, which is exactly the
    number the memory budget is about. ``cpu_percent`` is read with no blocking
    interval: it reports the average since the previous call, which is the right
    behaviour for a scrape-backed series.
    """
    memory = _process.memory_info()
    return ProcessMetrics(
        rss_bytes=memory.rss,
        cpu_percent=_process.cpu_percent(interval=None),
        threads=_process.num_threads(),
        uptime_seconds=max(time.time() - _started_at, 0.0),
    )


def current_index_stats() -> IndexStats | None:
    """Index statistics, or ``None`` when nothing is indexed yet."""
    if not dependencies.index_is_open():
        return None

    module, _ = dependencies.try_load_search_core()
    if module is None:
        return None

    stats = read_index_stats(module, get_settings().index_dir)
    return IndexStats(**stats) if stats is not None else None


@router.get("/health", response_model=HealthResponse)
def health() -> HealthResponse:
    """Liveness probe.

    Always answers 200 while the process is serving. It reports whether the Rust
    extension loaded and whether the index is open, not whether the process is
    alive: a probe that failed on a missing extension or an empty index would
    restart a container that is behaving correctly and would hide the real
    reason.
    """
    module, reason = dependencies.try_load_search_core()
    if module is None:
        return HealthResponse(
            status="degraded", extension_version=None, detail=reason, index_open=False
        )

    if not dependencies.index_is_open():
        return HealthResponse(
            status="degraded",
            extension_version=module.version(),
            detail=dependencies.index_open_error(),
            index_open=False,
        )

    return HealthResponse(status="ok", extension_version=module.version(), index_open=True)


@router.get("/metrics", response_model=MetricsResponse)
def metrics() -> MetricsResponse:
    """Resource metrics for this process, plus index statistics."""
    return MetricsResponse(process=process_metrics(), index=current_index_stats())
