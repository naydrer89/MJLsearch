"""The dashboard's single aggregate endpoint.

## What is global and what is per-process

This matters for reading the numbers honestly, so the endpoint reports only what
it can report truthfully:

* **Index, crawler and spool state are global.** They are files on disk written by
  other processes, so every server sees the same values.
* **Query analytics and process metrics are per-process.** uvicorn workers do not
  share memory, so a two-worker deployment has two query logs. The counters here
  describe the process that answered. Running the dashboard with one worker keeps
  the panels coherent, which is why ``scripts/run_dashboard.sh`` does that.

The alternative -- a shared counter in Redis or similar -- would add a service
dependency and network round trips on the query path to make a dashboard number
prettier. Not worth it.
"""

from __future__ import annotations

import logging
import time
from typing import Annotated

from fastapi import APIRouter, Depends

from api import dependencies
from api.config import get_settings
from api.models.responses import (
    AnalyticsOverview,
    CrawlerStats,
    GrowthStats,
    QueryStats,
    SpoolStats,
)
from api.routers.health import current_index_stats, process_metrics
from api.services.index_state import read_crawl_stats, read_index_history, read_spool_backlog

logger = logging.getLogger(__name__)

router = APIRouter(prefix="/analytics", tags=["analytics"])


def _crawler_stats() -> CrawlerStats | None:
    """The crawler's published progress, or ``None`` if it has never run."""
    stats = read_crawl_stats(get_settings().crawl_stats)
    if stats is None:
        return None

    # `updated_at` is an integer second in the crawler's file, and the model
    # validates it, so the fields are filtered rather than splatted: an extra key
    # in the crawler's JSON must not 500 the dashboard.
    known = {
        key: stats[key]
        for key in CrawlerStats.model_fields
        if key in stats and stats[key] is not None
    }
    return CrawlerStats(**known)


@router.get("/overview", response_model=AnalyticsOverview)
def overview() -> AnalyticsOverview:
    """Everything the dashboard renders, in one response.

    One request rather than six: the dashboard refreshes on a timer, and six
    round trips per tick would put six times the request pressure on the very
    process being measured.
    """
    settings = get_settings()
    return AnalyticsOverview(
        generated_at=time.time(),
        index=current_index_stats(),
        crawler=_crawler_stats(),
        spool=SpoolStats(**read_spool_backlog(settings.spool_dir)),
        queries=QueryStats(**dependencies.query_log().snapshot()),
        process=process_metrics(),
    )


@router.get("/growth", response_model=GrowthStats)
def growth() -> GrowthStats:
    """Index growth, for the always-visible footer and the stats page.

    Its own endpoint rather than part of ``/overview`` because it is polled on a
    different cadence: the footer refreshes every few seconds and must stay cheap,
    while ``/overview`` walks the spool directory and reads the crawler's file.

    An index that has never been drained answers ``available: false`` rather than
    zeroes, so the panels can say "nothing indexed yet" instead of drawing a flat
    line that looks like a stall.
    """
    history = read_index_history(get_settings().index_history)
    if history is None:
        return GrowthStats(available=False, path=str(get_settings().index_history))
    return GrowthStats(available=True, **history)


@router.post("/reset", status_code=204)
def reset_queries(
    request_id: Annotated[str, Depends(dependencies.get_request_id)],
) -> None:
    """Clears the query log.

    The one write this service offers, and it touches only its own memory: no
    index, no crawler state, nothing another process owns. That is why it is
    allowed from the dashboard while reindexing is not.
    """
    dependencies.query_log().clear()
    logger.info("query log cleared", extra={"request_id": request_id})
