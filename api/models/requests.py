"""Request contracts."""

from __future__ import annotations

from pydantic import BaseModel, Field


class SearchParams(BaseModel):
    """Validated query parameters for ``GET /search``.

    Modeled as a FastAPI dependency so that validation lives in one place rather
    than being spread across endpoint signatures.
    """

    q: str = Field(
        ...,
        min_length=1,
        max_length=512,
        description="Query string, using Tantivy query syntax.",
    )
    limit: int = Field(
        default=10,
        ge=1,
        description="Maximum number of results. Capped against SEARCH_MAX_LIMIT.",
    )
    # Documented as what it became rather than what it looks like. The ranking policy may
    # hand back a page shorter than `limit` -- the per-site cap and the end of the candidate
    # pool both do that -- so `offset // limit` addresses a *page*, and a caller stepping in
    # strides of `limit` would skip the results that fell between two pages. The response's
    # `pages` is the count to enumerate: offset = (page - 1) * limit is correct for every
    # page it reports.
    offset: int = Field(
        default=0,
        ge=0,
        description=(
            "Where to start: `offset // limit` selects the page. Pages can be shorter than "
            "`limit`, so enumerate with the response's `pages` rather than by striding."
        ),
    )
