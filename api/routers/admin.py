"""Read-only administrative endpoints.

There is deliberately no reindexing endpoint, and that is a design decision
rather than an omission. Reindexing is CLI-only because it is the one operation
that can replace or damage the index, and because the API process holds the index
read-only: Tantivy permits exactly one writer, and on this system that writer is
the ``indexer`` binary. Exposing it over HTTP would mean either giving the API a
second writer or having it shell out to one, and both trade a security and
consistency boundary for convenience.
"""

from __future__ import annotations

import logging
from typing import Annotated

from fastapi import APIRouter, Depends, HTTPException, status

from api import dependencies
from api.config import get_settings
from api.models.responses import IndexStats
from api.routers.health import current_index_stats

logger = logging.getLogger(__name__)

router = APIRouter(prefix="/admin", tags=["admin"])


@router.get("/index/stats", response_model=IndexStats)
def index_stats(
    request_id: Annotated[str, Depends(dependencies.get_request_id)],
) -> IndexStats:
    """Returns read-only statistics about the index."""
    settings = get_settings()
    module, reason = dependencies.try_load_search_core()
    if module is None:
        raise HTTPException(status_code=status.HTTP_503_SERVICE_UNAVAILABLE, detail=reason)

    stats = current_index_stats()
    if stats is None:
        raise HTTPException(
            status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
            detail=(
                "index is not open: "
                f"{dependencies.index_open_error() or 'no index at ' + str(settings.index_dir)}"
            ),
        )

    logger.info(
        "index stats served",
        extra={"request_id": request_id, "doc_count": stats.doc_count},
    )
    return stats
