"""Application settings.

Every value comes from the environment, optionally seeded from a ``.env`` file.
Nothing secret is ever defaulted in code, and the same variable names are read by
the Rust binaries, so a container needs no separate configuration format.
"""

from __future__ import annotations

from functools import lru_cache
from pathlib import Path

from pydantic import Field
from pydantic_settings import BaseSettings, SettingsConfigDict


class Settings(BaseSettings):
    """Runtime configuration for the API and dashboard processes."""

    model_config = SettingsConfigDict(
        env_file=".env",
        env_prefix="SEARCH_",
        extra="ignore",
    )

    # Single shard for now, but the shard-000 component is part of the path on
    # purpose: adding shards later is then a configuration change rather than a
    # data migration.
    index_dir: Path = Path("data/index/shard-000")

    # Where the crawler writes its progress file and where it seals spool
    # segments. Both are read here rather than queried: the crawler holds an
    # exclusive lock on its RocksDB directory, so asking it is not an option.
    #
    # Named `crawl_stats`, not `crawl_stats_path`, because pydantic-settings
    # derives the variable name from the field name: `crawl_stats_path` would be
    # read from `SEARCH_CRAWL_STATS_PATH` and would silently ignore the
    # `SEARCH_CRAWL_STATS` that the crawler binary actually writes.
    crawl_stats: Path = Path("data/crawl-stats.json")
    spool_dir: Path = Path("data/spool")
    # The index's growth timeline, written by the indexer. A separate file from the
    # crawl stats because it answers a different question: the crawl file says what
    # is being fetched, this one says what has actually become searchable.
    index_history: Path = Path("data/index-history.json")
    # Where the indexer keeps cross-site inlink counts between runs. Exposed so the
    # dashboard can report how much of the authority signal exists yet.
    host_authority: Path = Path("data/host-authority.json")

    host: str = "127.0.0.1"
    port: int = 8000
    dashboard_port: int = 5000
    # The search page. Separate from the dashboard on purpose: one is what a
    # visitor uses, the other is what an operator watches, and neither should be
    # able to take the other down.
    search_port: int = 5001
    log_level: str = "INFO"

    # Where the dashboard reads this API from. The dashboard never opens the
    # index itself: it is a viewer, and a viewer that shares a process with the
    # thing it is watching cannot report on that thing being down.
    backend_url: str = "http://127.0.0.1:8000"
    # The MJL Platform frontend that proxies into this API. Not used by the
    # API itself; it documents the deployment and keeps the compose file honest.
    mjl_platform_url: str = "http://127.0.0.1:3000"
    # Seconds the dashboard waits for the API before showing an unreachable
    # state. Short on purpose: a stalled dashboard is worse than an honest error.
    backend_timeout_seconds: float = Field(default=3.0, gt=0)
    poll_ms: int = Field(default=2000, ge=250)

    default_limit: int = Field(default=10, ge=1)
    max_limit: int = Field(default=100, ge=1)
    # Result pages kept in the engine's LRU. Each entry is ten hits of title,
    # snippet and score -- a few hundred bytes to a couple of kilobytes -- so this
    # is the one knob that trades RAM for latency, and it is bounded by design.
    query_cache_entries: int = Field(default=512, ge=1)
    # Touch the index at start-up so the first real query does not pay for a cold
    # mmap. Worth turning off only for a process that may never serve a query.
    warm_index: bool = True

    # Query post-processing. Exporting the policy as settings rather than
    # constants keeps a re-ranking change a restart instead of a deploy.
    ranking_enabled: bool = True
    freshness_half_life_days: float = Field(default=30.0, gt=0)
    max_per_domain: int = Field(default=2, ge=1)

    # Importance ranking. These are the knobs that decide whether a site's front
    # page beats its privacy policy, which is a question text relevance cannot
    # answer on its own: on a real site the legal pages often repeat the site's
    # name more often than the front page does.
    #
    # The three weights are in score units, tuned against BM25 scores that land
    # between about 2 and 10 on this corpus. The penalties are multipliers.
    rank_authority_weight: float = Field(default=2.5, ge=0)
    rank_host_authority_weight: float = Field(default=1.5, ge=0)
    rank_title_weight: float = Field(default=2.0, ge=0)
    rank_legal_penalty: float = Field(default=0.3, gt=0, le=1)
    rank_locale_penalty: float = Field(default=0.55, gt=0, le=1)
    # A site's own search box (`/search`, `?s=`, `?search=artemis`). Any query makes
    # another one, so these are not documents -- and their text is whatever was
    # searched for, which is how an empty CERN search page came up second for a
    # topic the site does not otherwise discuss.
    rank_search_view_penalty: float = Field(default=0.3, gt=0, le=1)
    rank_depth_decay: float = Field(default=0.93, gt=0, le=1)
    # Navigational intent: what a query that *names a site* is worth. The root weight
    # is the one that decides whether typing a company's name returns its front page
    # or its download page, which text relevance alone gets wrong on this corpus.
    rank_site_root_weight: float = Field(default=8.0, ge=0)
    rank_site_host_weight: float = Field(default=1.2, ge=0)
    # The domain cap can only drop results from what it is shown, so a page built
    # from exactly `limit` candidates comes back short whenever one host owns them
    # all: ask for ten, get two. The fix is to over-fetch candidates, cap those,
    # and then take the page. Bounded on both sides, because every extra candidate
    # is another snippet the engine has to generate.
    candidate_overfetch: int = Field(default=4, ge=1)
    # The bound the navigational widening asks for, and it is set by a measurement rather
    # than a round number: `github.com/` sits 358th of 642 in the engine's order for the
    # query `github`, so a window of 250 provably cannot contain the page that answers it.
    # Only a navigational query ever asks for this many, which is what keeps the engine's
    # LRU entries for ordinary queries small.
    max_candidates: int = Field(default=400, ge=1)
    # A floor under the over-fetch, and the reason it exists is worth stating: the
    # ranking layer can only promote what it is shown, and a navigational query -- one
    # that names a site -- is exactly the case where the page that should win sits deep
    # in the engine's own order. Measured on this corpus, the front pages of python.org,
    # docker.com, blender.org, debian.org and postgresql.org sat at BM25 ranks 80, 24,
    # 25, 25 and 23 for their own names, so a window of `limit * 4` never contained
    # them and the navigational bonus had nothing to lift.
    #
    # The cost is measured rather than assumed: 120 candidates answer in 0.38 ms against
    # 0.10 ms for 12, because the query itself is cheap and the per-hit work is a snippet
    # over a short field.
    min_candidates: int = Field(default=150, ge=1)

    # Analytics kept in this process. The bound is the point: an unbounded query
    # log would be exactly the kind of structure that grows with traffic and is
    # never reclaimed.
    query_log_size: int = Field(default=512, ge=0)

    # Plays a chime when the process starts, so a backgrounded start is audible.
    # Deduplicated across workers: see api.sound.
    startup_sound: bool = True
    sound_dir: Path = Path("data/sounds")
    sound_window_seconds: float = Field(default=5.0, ge=0)

    # Empty by default: these services are not meant to be browser-facing, so
    # CORS stays closed unless it is opened deliberately.
    cors_origins: list[str] = Field(default_factory=list)


@lru_cache
def get_settings() -> Settings:
    """Returns the process-wide settings, parsed once."""
    return Settings()
