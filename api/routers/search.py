"""The query endpoint."""

from __future__ import annotations

import logging
import time
from typing import Annotated, Any

from fastapi import APIRouter, Depends, HTTPException, status

from api import dependencies
from api.config import get_settings
from api.models.requests import SearchParams
from api.models.responses import RankingInfo, SearchHit, SearchResponse
from api.services import ranking

logger = logging.getLogger(__name__)

router = APIRouter(tags=["search"])


def _candidates_for(needed: int, settings: Any) -> int:
    """How many candidates to pull from the engine for a page of `needed` results.

    Two different reasons to look past `limit`, and both are about what post-processing
    can see rather than about paging:

    * the domain cap can only drop results it is shown, so a page built from exactly
      `limit` candidates comes back short whenever one host owns them all -- ask for ten,
      get two;
    * the ranking layer can only promote results it is shown, and a navigational query is
      the case where the page that should win sits deepest in the engine's own order.
      A floor is what puts a named site's front page in front of the re-ranker at all.

    Bounded on both sides, because each extra candidate is another snippet the engine
    generates and another hit an LRU entry holds.
    """
    return min(
        max(needed * settings.candidate_overfetch, needed, settings.min_candidates),
        settings.max_candidates,
    )


def _widen_for_navigation(
    module: Any,
    query: str,
    raw: dict[str, Any],
    candidates: int,
    settings: Any,
    request_id: str,
) -> tuple[dict[str, Any], int]:
    """Re-asks a navigational query with the widest window, and only a navigational one.

    The measurement that forces this: for the query `github`, the page that answers it --
    `github.com/` -- sits 358th of 642 in the engine's order, because a front page is
    long, marketing-shaped text in which the site's own name is diluted. No affordable
    floor reaches that, so the window is widened -- for this query only.

    Whether it *is* a navigation is decided from what the first window revealed: if any
    candidate belongs to a site the query names, then the query names a site. Ordinary
    queries never take this path, so they keep the small window and, with it, the small
    cache entry that lives in the engine's LRU.

    Returns the window **actually asked for** alongside the response, and the caller needs
    that second value: the short-page retry below decides whether a wider ask is worth
    paying for by comparing against the widest window, and after this function has already
    asked for it, a retry would be the same call with the same arguments. It is not free
    -- a wide window is the most expensive call the engine makes (measured: 99 ms for 400
    candidates against 45 ms for the 150-candidate first window), the cache does not
    absorb it while the crawler is committing, and it re-runs the ranking pass over 400
    hits on the Python side.
    """
    if candidates >= settings.max_candidates:
        return raw, candidates

    target = ranking.navigational_target(ranking.query_terms(query))
    if target is None:
        return raw, candidates

    labels = {ranking.host_label(hit["url"]) for hit in raw["results"]}
    if target not in labels:
        return raw, candidates

    logger.info(
        "widening a navigational query",
        extra={
            "request_id": request_id,
            "query": query,
            "site": target,
            "candidates": settings.max_candidates,
        },
    )
    widened = module.search(
        query, limit=settings.max_candidates, offset=0, request_id=request_id
    )
    return widened, settings.max_candidates


def _record_failure(query: str, started: float) -> None:
    """Logs a failed query in the analytics buffer, with the time it took to fail.

    Extracted so that the two failure branches cannot drift: they report the same event
    and only differ in the status code they raise afterwards.
    """
    dependencies.query_log().record(
        query=query,
        took_ms=(time.perf_counter() - started) * 1_000,
        total_matches=0,
        returned=0,
        failed=True,
    )


def _hits_from_extension(results: list[dict[str, Any]]) -> list[SearchHit]:
    """Converts the extension's plain dicts into the response model.

    Done explicitly rather than by relying on pydantic's coercion so that a
    malformed hit fails here, with the offending payload visible, instead of
    surfacing as a validation error somewhere further out.
    """
    return [
        SearchHit(
            url=hit["url"],
            title=hit["title"],
            snippet=hit["snippet"],
            fetched_at=int(hit["fetched_at"]),
            score=float(hit["score"]),
            # Defaulted rather than required: an index written before authority
            # existed has no such field, and a missing ranking signal should
            # degrade the ordering, not fail the query.
            authority=float(hit.get("authority", 0.0)),
            host_authority=float(hit.get("host_authority", 0.0)),
        )
        for hit in results
    ]


@router.get("/search", response_model=SearchResponse)
def search(
    params: Annotated[SearchParams, Depends()],
    request_id: Annotated[str, Depends(dependencies.get_request_id)],
) -> SearchResponse:
    """Runs a query against the index, then applies ranking policy to the page.

    Two timing numbers exist -- the engine's own and the wall clock here -- and
    the one reported is the wall clock, because that is what a caller waits for.
    The engine's figure still reaches the analytics log, where the difference
    between the two is what would expose an expensive post-processing pass.
    """
    settings = get_settings()

    if params.limit > settings.max_limit:
        raise HTTPException(
            status_code=status.HTTP_422_UNPROCESSABLE_CONTENT,
            detail=f"limit must not exceed {settings.max_limit}",
        )

    module, reason = dependencies.try_load_search_core()
    if module is None:
        raise HTTPException(status_code=status.HTTP_503_SERVICE_UNAVAILABLE, detail=reason)

    if not dependencies.index_is_open():
        # A distinct case from a missing extension: the server is healthy, there
        # is simply nothing indexed yet. Saying so is what tells the operator to
        # run the crawler and the indexer.
        raise HTTPException(
            status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
            detail=(
                "index is not open: "
                f"{dependencies.index_open_error() or 'no index at ' + str(settings.index_dir)}"
            ),
        )

    needed = params.offset + params.limit
    candidates = _candidates_for(needed, settings)

    started = time.perf_counter()
    try:
        # Always from offset zero: the engine's own offset would slice before the policy
        # ran, so a page could come back empty purely because ranking moved things out of
        # the window the engine had chosen.
        raw = module.search(
            params.q,
            limit=candidates,
            offset=0,
            request_id=request_id,
        )
        raw, window = _widen_for_navigation(
            module, params.q, raw, candidates, settings, request_id
        )
    except (ValueError, RuntimeError) as error:
        _record_failure(params.q, started)
        # Two failures with the same handling and different meaning. A query the parser
        # rejected is the caller's mistake, so it is a 400 and the parser's own message
        # says what was wrong with the syntax; anything else the engine raises is ours, so
        # it is a 500 and it is logged. Keeping them in one clause is what stops the
        # bookkeeping above from being written twice and drifting apart.
        if isinstance(error, ValueError):
            raise HTTPException(
                status_code=status.HTTP_400_BAD_REQUEST,
                detail=f"invalid query: {error}",
            ) from error
        logger.error(
            "query failed",
            extra={"request_id": request_id, "query": params.q, "detail": str(error)},
        )
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail=f"query failed: {error}",
        ) from error

    engine_ms = float(raw["took_ms"])
    cached = bool(raw["cached"])
    relaxed = bool(raw["relaxed"])
    total_matches = int(raw["total_matches"])

    def pool_of(payload: dict[str, Any]) -> tuple[list[SearchHit], RankingInfo | None]:
        """The ordered candidate pool for one engine response, as a reusable step.

        A closure rather than a module function because it reads a dozen settings and the
        query, and threading those through a signature would be more code than the call
        it replaces. It exists as a step at all so the retry below can rank a second
        window without a second copy of this block.
        """
        window = _hits_from_extension(payload["results"])
        if not (settings.ranking_enabled and window):
            return window, None

        pool = ranking.ordered_pool(
            window,
            now=int(time.time()),
            query=params.q,
            half_life_days=settings.freshness_half_life_days,
            authority_weight=settings.rank_authority_weight,
            host_authority_weight=settings.rank_host_authority_weight,
            title_weight=settings.rank_title_weight,
            legal_penalty=settings.rank_legal_penalty,
            locale_penalty=settings.rank_locale_penalty,
            search_view_penalty=settings.rank_search_view_penalty,
            depth_decay=settings.rank_depth_decay,
            site_root_weight=settings.rank_site_root_weight,
            site_host_weight=settings.rank_site_host_weight,
        )
        info = RankingInfo(
            freshness_half_life_days=settings.freshness_half_life_days,
            max_per_domain=settings.max_per_domain,
            # Duplicates only: the per-site cap no longer removes anything from the pool,
            # it decides what fits on one page, so counting its skips here would report
            # results as lost that are still reachable on the next page.
            suppressed=len(window) - len(pool),
            candidates=len(window),
            authority_weight=settings.rank_authority_weight,
            host_authority_weight=settings.rank_host_authority_weight,
            title_weight=settings.rank_title_weight,
            site_root_weight=settings.rank_site_root_weight,
            site_host_weight=settings.rank_site_host_weight,
        )
        return pool, info

    def page_of(pool: list[SearchHit]) -> ranking.Page:
        """The requested page out of an ordered pool, capped per site *per page*.

        With ranking off there is no pool worth speaking of and no policy in force, so the
        window is sliced directly -- the same behaviour as before, and the one case where
        a caller has explicitly asked for no post-processing. The page count is then the
        arithmetic one, because a slice cannot come up short.
        """
        if not settings.ranking_enabled:
            return ranking.Page(
                results=pool[params.offset : params.offset + params.limit],
                held_back=0,
                pages=-(-len(pool) // params.limit) if pool else 0,
            )
        return ranking.take_page(
            pool,
            offset=params.offset,
            limit=params.limit,
            max_per_domain=settings.max_per_domain,
        )

    pool, rank_info = pool_of(raw)
    page_selection = page_of(pool)

    # A page that came up short while matches remain is the ranking policy working on a
    # window a few hosts own: ask for ten and get six, with nineteen hundred matching,
    # because the cap only ever saw candidates it was shown. One wider ask fixes it, and
    # it is asked for here -- after seeing that the page *is* short -- rather than guessed
    # at beforehand from the window's shape. Note what it is *not* for: it cannot rescue a
    # deep page, because a window of 400 candidates is what bounds paging. That bound is
    # what `reachable` reports instead of hiding.
    #
    # `window`, not `candidates`: a navigational query has already been asked for the widest
    # window by now, so retrying on the *first* window's size would repeat that exact call.
    if (
        len(page_selection.results) < params.limit
        and window < settings.max_candidates
        and total_matches > window
    ):
        wider = module.search(
            params.q, limit=settings.max_candidates, offset=0, request_id=request_id
        )
        window = settings.max_candidates
        wider_pool, wider_info = pool_of(wider)
        wider_selection = page_of(wider_pool)
        if len(wider_selection.results) > len(page_selection.results):
            logger.info(
                "widened a short page",
                extra={
                    "request_id": request_id,
                    "query": params.q,
                    "had": len(page_selection.results),
                    "now": len(wider_selection.results),
                    "candidates": settings.max_candidates,
                },
            )
            pool, page_selection, rank_info, raw = (
                wider_pool,
                wider_selection,
                wider_info,
                wider,
            )
            engine_ms = float(raw["took_ms"])
            cached = bool(raw["cached"])
            relaxed = bool(raw["relaxed"])

    page = page_selection.results

    # What paging can reach, as opposed to what matched: `total` counts the index, these
    # two count what the policy can hand out. The pool is the window the engine was asked
    # for, so a pager built from `total` would offer pages that cannot exist -- which it
    # did, to the tune of 1 172 pages with the second one empty.
    reachable = len(pool)
    if rank_info is not None:
        rank_info.capped_on_page = page_selection.held_back

    took_ms = (time.perf_counter() - started) * 1_000
    dependencies.query_log().record(
        query=params.q,
        took_ms=took_ms,
        total_matches=total_matches,
        returned=len(page),
        cached=cached,
    )

    logger.info(
        "search served",
        extra={
            "request_id": request_id,
            "query": params.q,
            "limit": params.limit,
            "offset": params.offset,
            "candidates": candidates,
            "window": window,
            "total": total_matches,
            "reachable": reachable,
            "returned": len(page),
            "cached": cached,
            "relaxed": relaxed,
            "engine_ms": round(engine_ms, 3),
            "took_ms": round(took_ms, 3),
        },
    )

    return SearchResponse(
        query=params.q,
        took_ms=took_ms,
        total=total_matches,
        reachable=reachable,
        pages=page_selection.pages,
        limit=params.limit,
        offset=params.offset,
        cached=cached,
        relaxed=relaxed,
        results=page,
        ranking=rank_info,
    )
