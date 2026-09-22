"""Response contracts."""

from __future__ import annotations

from typing import Literal

from pydantic import BaseModel, Field


class SearchHit(BaseModel):
    """One search result."""

    url: str
    title: str
    snippet: str = Field(description="Matched fragment as HTML, matched terms in <b>.")
    fetched_at: int = Field(description="Unix timestamp in seconds.")
    score: float = Field(description="Relevance after ranking post-processing.")
    # The link-graph scores the indexer computed, returned so a caller can see why
    # a result is where it is. They are inputs to `score`, not a second opinion on
    # it: a page can be authoritative and still rank low because it barely matches.
    authority: float = Field(
        default=0.0, description="How linked-to the page is within the crawled graph, in [0, 1]."
    )
    host_authority: float = Field(
        default=0.0, description="How many other sites link to this page's host, in [0, 1]."
    )


class RankingInfo(BaseModel):
    """What post-processing did to the result set.

    Reported rather than silent: a caller comparing two responses needs to know
    whether domain diversity dropped results, or the counts look inconsistent
    with the ranking order.
    """

    freshness_half_life_days: float
    max_per_domain: int
    suppressed: int = Field(
        description=(
            "Candidates dropped because they were the same page under another URL. "
            "Global to the pool, not per page."
        )
    )
    # How many candidates the per-page cap held back while this page was filled. Page
    # scoped on purpose: the cap keeps one site from taking the ten slots the visitor is
    # looking at, so the same candidates are still reachable on a later page, and a
    # number that counted them as lost would be wrong about that.
    capped_on_page: int = Field(
        default=0,
        description="Candidates the per-site cap held back from this page.",
    )
    # How many candidates the policy saw. Exposed because "3 results out of 1709" is
    # otherwise indistinguishable from a broken query: with the number of candidates in
    # hand, it reads as what it is -- a ranking policy doing its job on a corpus where one
    # host owns everything.
    candidates: int = Field(
        default=0, description="How many candidates the ranking policy was given."
    )
    # The weights in force, reported for the same reason the half-life is: an
    # ordering that looks wrong is usually a weight that is not what the operator
    # assumed, and reading it out of the response beats reading it out of the code.
    authority_weight: float = Field(default=0.0, description="Weight on the page's link authority.")
    host_authority_weight: float = Field(
        default=0.0, description="Weight on the host's cross-site authority."
    )
    title_weight: float = Field(default=0.0, description="Weight on how much the title matches.")
    site_root_weight: float = Field(
        default=0.0, description="Bonus for the front page of a site the query names."
    )
    site_host_weight: float = Field(
        default=0.0, description="Bonus for pages of a site the query names."
    )


class SearchResponse(BaseModel):
    """Response body for ``GET /search``."""

    query: str
    took_ms: float = Field(description="Server-side time spent executing the query.")
    total: int = Field(description="Matching documents in the index, before diversity caps.")
    # `total` counts what matched in the whole index; `reachable` counts what paging can
    # actually hand out, which is the size of the ranked candidate window the policy
    # worked on. The two differ by orders of magnitude on a real corpus, and a pager built
    # from `total` therefore offers pages that come back empty -- 13 816 matches but eight
    # reachable results is the measured case that produced this field. Clients should page
    # against `reachable` and show `total` as "about N results".
    reachable: int = Field(
        default=0,
        description="Results paging can reach: the size of the ranked candidate window.",
    )
    # How many pages that pool produces *at this limit*, walked rather than divided. Not
    # `ceil(reachable / limit)`: a page is allowed to hold fewer than `limit` results when
    # the per-site cap or the end of the pool leaves it short, so the estimate undercounts
    # and a pager built from it stops a visitor early.
    pages: int = Field(
        default=0,
        description="Pages this pool can produce at the requested limit.",
    )
    limit: int
    offset: int
    cached: bool = Field(description="Whether the query engine answered from its LRU cache.")
    relaxed: bool = Field(
        default=False,
        description=(
            "Whether every term was required to match. False means the strict "
            "search found nothing and the results are approximate."
        ),
    )
    results: list[SearchHit]
    ranking: RankingInfo | None = None


class HealthResponse(BaseModel):
    """Response body for ``GET /health``."""

    status: Literal["ok", "degraded"]
    extension_version: str | None = None
    detail: str | None = Field(default=None, description="Why the service is degraded, when it is.")
    index_open: bool = Field(default=False, description="Whether the index handle is mapped.")


class ProcessMetrics(BaseModel):
    """Resource use of this API process."""

    rss_bytes: int = Field(description="Resident set size, read via psutil.")
    cpu_percent: float
    threads: int
    uptime_seconds: float = 0.0


class IndexStats(BaseModel):
    """Read-only statistics about the index."""

    doc_count: int
    index_bytes: int = Field(description="Bytes on disk, which is also what is mmap'd.")
    path: str
    fingerprint: int | None = Field(
        default=None,
        description=(
            "Hash of the current segment set. Changes only when a commit alters the "
            "index, so it is both a cache stamp and a 'has anything changed' signal."
        ),
    )
    cache_entries: int | None = Field(
        default=None, description="Queries held in the engine's LRU cache."
    )


class MetricsResponse(BaseModel):
    """Response body for ``GET /metrics``."""

    process: ProcessMetrics
    index: IndexStats | None = Field(
        default=None,
        description="Null until the index has been opened and contains something.",
    )


class CrawlerStats(BaseModel):
    """Progress published by the crawler process."""

    queued: int = 0
    fetched: int = 0
    indexed: int = 0
    failed: int = 0
    disallowed: int = 0
    frontier_pending: int = 0
    hosts: int = 0
    updated_at: int | None = None
    age_seconds: float | None = Field(
        default=None, description="Seconds since the crawler last wrote this file."
    )
    stale: bool = Field(default=True, description="True when the crawler has not written recently.")


class SpoolStats(BaseModel):
    """The crawler-to-indexer hand-off as seen from the filesystem."""

    sealed_segments: int = Field(
        description="Segments waiting for or being drained by the indexer."
    )
    sealed_bytes: int = 0
    open_segments: int = Field(
        default=0, description="The crawler's in-progress buffers, which are not a backlog."
    )
    path: str = ""


class GrowthBucket(BaseModel):
    """Documents the index gained during one minute.

    Gained, not written: re-indexing a page that is already there is work and no growth,
    so the buckets sum to no more than `total_documents`.
    """

    at: int = Field(description="Start of the minute, unix seconds.")
    documents: int


class GrowthStats(BaseModel):
    """How fast the index is growing, and how much is expected next.

    Every number here comes from the indexer's own timeline rather than from a
    counter that would have to be differenced against an earlier reading, so the
    figures are correct even for a server that started a moment ago.
    """

    available: bool = Field(
        default=False, description="False until the indexer has recorded a first commit."
    )
    path: str = ""
    updated_at: int | None = None
    age_seconds: float | None = None
    idle: bool = Field(
        default=True, description="Nothing has been indexed recently, so the rate is zero."
    )
    total_documents: int = 0
    documents_last_hour: int = Field(
        default=0, description="Net new documents in the last hour. What the footer reports."
    )
    documents_last_quarter: int = 0
    rate_per_minute: float = 0.0
    projected_next_hour: int = Field(
        default=0, description="Current rate extended over an hour. A projection, not a promise."
    )
    buckets: list[GrowthBucket] = Field(
        default_factory=list, description="The last hour, one entry per minute that saw a commit."
    )


class LatencyStats(BaseModel):
    """Latency distribution over the retained queries."""

    mean: float = 0.0
    p50: float = 0.0
    p95: float = 0.0
    max: float = 0.0


class TopQuery(BaseModel):
    """A query and how often it was served."""

    query: str
    count: int


class SlowQuery(BaseModel):
    """One of the slowest retained queries."""

    query: str
    took_ms: float
    total_matches: int


class RecentQuery(BaseModel):
    """One entry of the recent-query feed."""

    query: str
    took_ms: float
    total_matches: int
    returned: int
    cached: bool
    failed: bool
    at: float
    age_seconds: float


class SeriesPoint(BaseModel):
    """One bucket of the throughput and latency series."""

    at: float
    count: int
    mean_ms: float
    max_ms: float


class QueryStats(BaseModel):
    """Aggregates over this process's bounded query log."""

    capacity: int
    retained: int
    total_recorded: int
    uptime_seconds: float
    cached: int
    cache_hit_rate: float
    failed: int
    matches: int
    returned: int
    latency_ms: LatencyStats
    queries_per_second: float
    series: list[SeriesPoint]
    top_queries: list[TopQuery]
    slowest: list[SlowQuery]
    recent: list[RecentQuery]


class AnalyticsOverview(BaseModel):
    """Everything the dashboard renders, in one response.

    One request rather than six: the dashboard refreshes on a timer, and six
    round trips per tick would put six times the request pressure on the very
    process being measured.
    """

    generated_at: float
    index: IndexStats | None = None
    crawler: CrawlerStats | None = None
    spool: SpoolStats
    queries: QueryStats
    process: ProcessMetrics
