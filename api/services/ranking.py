"""Post-processing of results returned by the Rust query engine.

Ranking policy deliberately lives here rather than in Rust. It changes far more
often than the query engine does, and at this layer the cost of a Python pass
over a page of results is negligible against the flexibility it buys.

The Rust side returns raw BM25 scores, the two link-graph scores it computed at
index time, and knows nothing about policy, so everything applied here is
additive and reversible.

## What decides the order

Four things, in this order, and each of them exists because the one before it
cannot do the job alone:

1. **BM25** decides *what matches*. Nothing here can invent a result.
2. **Freshness** decays a score by document age, so a page about a moving target
   does not outrank a newer one.
3. **Authority** — from the indexer's link graph — is added on top. This is what
   puts a site's front page above a page on the same site that merely mentions
   the same words more often.
4. **URL quality and title match** are priors about shape rather than content: a
   privacy policy is not what someone searching for a company's name meant, and
   a page whose title *is* the query is usually the page that answers it.

Step 3 and 4 are the reason a query for a company's name returns its home page
first rather than whatever page repeats the name most: on a real site the legal
pages often mention the name more often than the front page does. Text relevance
cannot tell those apart. Link structure and URL shape can, and both are used.

All of these are pure functions, so the ordering a policy produces can be
asserted rather than eyeballed.
"""

from __future__ import annotations

import math
import re
from collections.abc import Iterable, Sequence
from dataclasses import dataclass

from api.models.responses import SearchHit

DEFAULT_HALF_LIFE_DAYS = 30.0
DEFAULT_MAX_PER_DOMAIN = 2
SECONDS_PER_DAY = 86_400.0

# How much each importance signal is worth, in score units. BM25 scores on this
# corpus land between about 2 and 10, which is the scale these are tuned against:
# authority can move a result past a slightly better textual match, but not past a
# far better one.
DEFAULT_AUTHORITY_WEIGHT = 2.5
DEFAULT_HOST_AUTHORITY_WEIGHT = 1.5
DEFAULT_TITLE_WEIGHT = 2.0

# Multipliers. Below one is a demotion, and the two site-furniture penalties are the
# strongest levers here: "privacy policy" beating "home page", and a site's empty search
# box beating the article about the thing being searched for, are the exact failures
# this layer was added to fix.
DEFAULT_LEGAL_PENALTY = 0.3
DEFAULT_LOCALE_PENALTY = 0.55
DEFAULT_SEARCH_VIEW_PENALTY = 0.3
DEFAULT_DEPTH_DECAY = 0.93
DEFAULT_DEPTH_FLOOR = 0.55
DEFAULT_HOME_BONUS = 1.15

# Navigational intent. When the query *names the site* -- `python` for python.org,
# `docker` for www.docker.com -- nothing about text relevance can settle which page the
# visitor wanted: the site's download page and its front page both mention the word, and
# on this corpus the download page matched better. What distinguishes them is that one
# of them is the site. So a query term that *is* the site's own name lifts that site:
# strongly for its root, which is what a navigation is asking for, and modestly for the
# rest of the site, because a named site's pages beat other sites' mentions of the same
# word.
#
# The root weight is large on purpose, and it is a measurement rather than a taste: a
# site's front page is often the *worst* textual match on it. Measured here,
# `github.com/` scores 3.31 for the query `github` while `docs.github.com/en` scores 9.31
# -- the homepage is a long marketing page where the site's own name is diluted, and the
# documentation index is short and repeats it. A bonus that nudges cannot win that; one
# that decides can, and for a query that is nothing but a site's name, deciding is the
# right behaviour. The window is what keeps it honest: this only fires when the root is
# a candidate at all.
DEFAULT_SITE_ROOT_WEIGHT = 8.0
DEFAULT_SITE_HOST_WEIGHT = 1.2

# Terms that are part of a written-out hostname rather than part of the subject. Someone
# typing `www.python.org` is still naming one site, and treating `www` and `org` as
# subject words would turn the clearest navigation there is into a three-term query.
NAVIGATIONAL_NOISE = frozenset({"www", "http", "https", "com", "org", "net", "io", "co"})

# Two-part public suffixes that appear on this corpus. Not a public-suffix list: the
# only job here is to find which label a host is *named* by, and being wrong about an
# exotic suffix costs a bonus rather than producing a wrong answer.
TWO_PART_SUFFIX = frozenset({"co", "com", "org", "net", "ac", "gov", "edu"})

# Words that mark a page as site furniture rather than content. Matched against
# whole hyphen-separated tokens of a path segment, so that `/privacy-policy` and
# `/policies/terms` both count while `/policyholders` does not.
LEGAL_TOKENS = frozenset(
    {
        "privacy",
        "terms",
        "legal",
        "cookie",
        "cookies",
        "policies",
        "policy",
        "eula",
        "gdpr",
        "imprint",
        "impressum",
        "disclaimer",
        "licenses",
        "licence",
        "license",
        "trademark",
        "accessibility",
    }
)

# A site's own search box, which is not a document. Any query generates another one of
# these, so they are unbounded, and their text belongs to whatever was searched for
# rather than to the site: `home.cern/?s=` ranked second for a topic query whose words
# CERN's empty search page mentions only because a search page lists its site's own
# navigation. Demoted with the same lever as the legal pages, and for the same reason —
# it is site furniture wearing a content-shaped URL.
#
# The path form is a whole-segment match (`/us/search`, `/clubs/search`), and the query
# form is a parameter *name* match, because `?search=Artemis` puts the site's search in
# the query of a URL whose path is `/`.
SEARCH_PATH_TOKENS = frozenset(
    {"search", "suche", "searchresults", "search-results", "suchergebnisse"}
)
SEARCH_QUERY_PARAMS = frozenset(
    {"s", "q", "query", "search", "searchterm", "search_term", "keyword", "keywords"}
)

# A path segment like `en-us` or `zh-hant`. A bare two-letter segment is *not*
# enough on its own — `/go/to` would match — so a bare one only counts inside an
# `intl` path, which is where it means a language.
REGION_TAG = re.compile(r"[a-z]{2}[-_][a-z0-9]{2,4}")
TOKEN = re.compile(r"[a-z0-9]+")


def _is_legal_segment(segment: str) -> bool:
    """Whether a path segment names site furniture rather than content.

    The legal word has to be the *first* token of the segment. Matching anywhere
    inside it would condemn `/blog/why-policies-matter`, which is an article about
    policies and exactly the kind of page that must not be demoted, while
    first-token matching still catches every spelling a site actually uses for a
    legal page: `/privacy`, `/privacy-policy`, `/legal/terms`, `/terms-of-service`,
    `/policies/cookies`, `/imprint`.
    """
    stem = segment.rsplit(".", 1)[0] if "." in segment else segment
    tokens = [part for part in re.split(r"[-_]", stem) if part]
    return bool(tokens) and tokens[0] in LEGAL_TOKENS


def _query_param_names(url: str) -> list[str]:
    """The parameter names in a URL's query string, lowercased.

    Split on `&` and `;` and cut at the first `=`, rather than parsed as form data:
    the only question here is what the parameters are *called*, and a hand-rolled
    split keeps a malformed escape from raising on a URL the crawler accepted.
    """
    path = url.split("://", 1)[-1]
    if "?" not in path:
        return []
    query = path.split("?", 1)[1]
    if not query:
        return []
    names: list[str] = []
    for pair in re.split(r"[&;]", query):
        if not pair:
            continue
        names.append(pair.split("=", 1)[0].strip().lower())
    return names


def is_search_view(url: str) -> bool:
    """Whether a URL addresses a site's own search results rather than a page."""
    if any(segment.rsplit(".", 1)[0] in SEARCH_PATH_TOKENS for segment in _path_segments(url)):
        return True
    return any(name in SEARCH_QUERY_PARAMS for name in _query_param_names(url))


def navigational_target(terms: Sequence[str]) -> str | None:
    """The site a query is naming, or ``None`` when the query is not a navigation.

    A navigation is a query that *is* a site's name and nothing else -- `github`,
    `www.python.org` -- which is why this returns nothing as soon as a second meaningful
    term appears: `github copilot` and `github actions` name a subject on a site rather
    than asking for the site, and promoting the front page for those would answer a
    question nobody asked.
    """
    meaningful = [term for term in terms if term not in NAVIGATIONAL_NOISE]
    if len(meaningful) != 1:
        return None
    return meaningful[0]


def _host(url: str) -> str:
    """Returns the lowercased host of a URL, or the whole string if unparsable."""
    without_scheme = url.split("://", 1)[-1]
    return without_scheme.split("/", 1)[0].split("?", 1)[0].lower()


def host_label(url: str) -> str:
    """The label a site is named by, for deciding whether a query is navigational.

    ``www.python.org`` is `python`, ``docs.github.com`` is `github`, ``kubernetes.io``
    is `kubernetes`. A leading `www.` is dropped because it is a hostname convention
    rather than a name, and subdomains collapse onto the site they belong to --
    `docs.github.com` is GitHub's documentation, which is what a query for `github`
    may well be after.
    """
    host = _host(url).split(":", 1)[0]
    if host.startswith("www."):
        host = host[4:]

    labels = [label for label in host.split(".") if label]
    if len(labels) >= 3 and labels[-2] in TWO_PART_SUFFIX:
        return labels[-3]
    if len(labels) >= 2:
        return labels[-2]
    return labels[0] if labels else ""


def _path_segments(url: str) -> list[str]:
    """The path segments of a URL, lowercased, without the query or fragment.

    Lowercasing here rather than at each use is load-bearing: `/en-US/docs` is a
    locale variant and `/en-us/docs` is the same thing, and a region tag is
    uppercase by convention.
    """
    path = url.split("://", 1)[-1]
    path = path.split("?", 1)[0].split("#", 1)[0]
    path = path.split("/", 1)[1] if "/" in path else ""
    return [segment.lower() for segment in path.split("/") if segment]


def query_terms(query: str) -> tuple[str, ...]:
    """Splits a query into the terms the ranking layer reasons about.

    Deliberately the same rough tokenisation the index uses, rather than a parser
    of its own: this decides how much a title matches, and being stricter here
    than the engine is would make the layer disagree with the results it ranks.
    """
    return tuple(TOKEN.findall(query.lower()))


def title_match(title: str, terms: Sequence[str]) -> float:
    """How much of a title the query accounts for, in `[0, 1]`.

    The denominator is the *title's* length rather than the query's, which is what
    separates a page called "Google" from one called "Google privacy policy" for
    the query `google`: both contain the term, and only one is mostly about it.
    """
    if not terms:
        return 0.0
    title_terms = TOKEN.findall(title.lower())
    if not title_terms:
        return 0.0

    covered = sum(1 for term in set(terms) if term in title_terms)
    coverage = covered / len(set(terms))
    # Bounded by one: a title shorter than the query still cannot over-credit.
    return min(1.0, coverage * len(set(terms)) / len(title_terms))


def url_quality(
    url: str,
    *,
    legal_penalty: float = DEFAULT_LEGAL_PENALTY,
    locale_penalty: float = DEFAULT_LOCALE_PENALTY,
    search_view_penalty: float = DEFAULT_SEARCH_VIEW_PENALTY,
    depth_decay: float = DEFAULT_DEPTH_DECAY,
    depth_floor: float = DEFAULT_DEPTH_FLOOR,
    home_bonus: float = DEFAULT_HOME_BONUS,
) -> float:
    """A multiplier in `(0, 1]` for what the URL's own shape says about the page.

    Nothing here looks at content, and that is the point: these are the signals
    that survive when every candidate page mentions the query equally often.
    """
    if legal_penalty <= 0 or locale_penalty <= 0 or search_view_penalty <= 0:
        raise ValueError("penalties must be positive")

    segments = _path_segments(url)
    quality = 1.0

    if any(_is_legal_segment(segment) for segment in segments):
        quality *= legal_penalty

    if "intl" in segments or any(REGION_TAG.fullmatch(segment) for segment in segments):
        quality *= locale_penalty

    if is_search_view(url):
        quality *= search_view_penalty

    # The root of a host is the page a search for that host's name means.
    if not segments:
        quality *= home_bonus
    elif len(segments) > 2:
        quality *= max(depth_floor, depth_decay ** (len(segments) - 2))

    return quality


def apply_importance(
    hits: Sequence[SearchHit],
    *,
    query: str = "",
    authority_weight: float = DEFAULT_AUTHORITY_WEIGHT,
    host_authority_weight: float = DEFAULT_HOST_AUTHORITY_WEIGHT,
    title_weight: float = DEFAULT_TITLE_WEIGHT,
    legal_penalty: float = DEFAULT_LEGAL_PENALTY,
    locale_penalty: float = DEFAULT_LOCALE_PENALTY,
    search_view_penalty: float = DEFAULT_SEARCH_VIEW_PENALTY,
    depth_decay: float = DEFAULT_DEPTH_DECAY,
    site_root_weight: float = DEFAULT_SITE_ROOT_WEIGHT,
    site_host_weight: float = DEFAULT_SITE_HOST_WEIGHT,
) -> list[SearchHit]:
    """Re-scores hits by authority, URL shape and how much the title matches.

    Replaces `score` with the adjusted value, so that sorting afterwards — and
    anything downstream that reads the score — sees the ranking that is actually
    in force rather than the raw BM25 figure.
    """
    terms = query_terms(query)
    unique_terms = set(terms)
    target = navigational_target(terms)
    adjusted: list[SearchHit] = []

    for hit in hits:
        segments = _path_segments(hit.url)
        quality = url_quality(
            hit.url,
            legal_penalty=legal_penalty,
            locale_penalty=locale_penalty,
            search_view_penalty=search_view_penalty,
            depth_decay=depth_decay,
        )
        # Multiplied rather than added, because these are claims about the *same*
        # page: a deep legal page is penalised twice, and that is intended.
        relevance = hit.score * quality
        importance = (
            authority_weight * hit.authority
            + host_authority_weight * hit.host_authority
            + title_weight * title_match(hit.title, terms)
        )

        # Navigational intent, weighed by where on the site the page sits.
        #
        # The two bonuses are alternatives rather than a sum, and the shape of the rule is
        # what makes `python docs` still return the documentation: a named site's *pages*
        # are lifted, but its front page is lifted only by a query that is nothing but the
        # site's name. "The front page is what I want" is a claim a navigation supports and
        # a two-term query does not.
        label = host_label(hit.url)
        if label and label in unique_terms and segments:
            importance += site_host_weight
        if label and label == target and not segments:
            # The root of the site the query names is the answer to a navigation, and the
            # root is exactly the page text relevance cannot pick out: it is the page that
            # repeats its own name least, because the rest of it is about everything else
            # the company does.
            importance += site_host_weight + site_root_weight

        adjusted.append(hit.model_copy(update={"score": relevance + importance}))

    return adjusted


def apply_freshness_boost(
    hits: Sequence[SearchHit],
    *,
    now: int,
    half_life_days: float = DEFAULT_HALF_LIFE_DAYS,
) -> list[SearchHit]:
    """Scales each score by an exponential decay on document age.

    The decay is applied as a multiplier in ``(0, 1]``, so a fresh document keeps
    its BM25 score and an old one is demoted rather than removed. A negative age
    (a clock skew, or a page dated in the future) is clamped to zero so it cannot
    be boosted above a same-day document.
    """
    if half_life_days <= 0:
        raise ValueError("half_life_days must be positive")

    decay_per_day = math.log(2) / half_life_days
    boosted: list[SearchHit] = []

    for hit in hits:
        age_days = max(0.0, (now - hit.fetched_at) / SECONDS_PER_DAY)
        factor = math.exp(-decay_per_day * age_days)
        boosted.append(hit.model_copy(update={"score": hit.score * factor}))

    return boosted


def sort_by_score(hits: Sequence[SearchHit]) -> list[SearchHit]:
    """Orders hits by descending score, keeping engine order among ties.

    A stable sort is required, not incidental: once the multipliers underflow to
    zero for a whole page, every score is equal and the only thing left deciding
    the order is the engine's own ranking. An unstable sort would shuffle that page
    on every request.
    """
    return sorted(hits, key=lambda hit: hit.score, reverse=True)


def ordered_pool(
    hits: Sequence[SearchHit],
    *,
    now: int,
    query: str = "",
    half_life_days: float = DEFAULT_HALF_LIFE_DAYS,
    authority_weight: float = DEFAULT_AUTHORITY_WEIGHT,
    host_authority_weight: float = DEFAULT_HOST_AUTHORITY_WEIGHT,
    title_weight: float = DEFAULT_TITLE_WEIGHT,
    legal_penalty: float = DEFAULT_LEGAL_PENALTY,
    locale_penalty: float = DEFAULT_LOCALE_PENALTY,
    search_view_penalty: float = DEFAULT_SEARCH_VIEW_PENALTY,
    depth_decay: float = DEFAULT_DEPTH_DECAY,
    site_root_weight: float = DEFAULT_SITE_ROOT_WEIGHT,
    site_host_weight: float = DEFAULT_SITE_HOST_WEIGHT,
) -> list[SearchHit]:
    """The whole policy except the caps: boosted, sorted, deduplicated.

    Split out from :func:`rank` because the two callers want different things from the
    same ordering. Paging needs the *pool* -- the complete ranked candidate list, caps
    not yet applied -- because a page is a slice of it, and the size of that pool is the
    number of results paging can actually reach. :func:`rank` still returns a capped
    pool for callers that want one.

    Boosting and *then* sorting is the part that is easy to get wrong: a boost that is
    never followed by a sort changes nothing at all, because the engine already returned
    its page in its own order. That is not a cosmetic detail -- it would make the entire
    policy a no-op while its configuration still looked effective.
    """
    freshness = apply_freshness_boost(hits, now=now, half_life_days=half_life_days)
    importance = apply_importance(
        freshness,
        query=query,
        authority_weight=authority_weight,
        host_authority_weight=host_authority_weight,
        title_weight=title_weight,
        legal_penalty=legal_penalty,
        locale_penalty=locale_penalty,
        search_view_penalty=search_view_penalty,
        depth_decay=depth_decay,
        site_root_weight=site_root_weight,
        site_host_weight=site_host_weight,
    )
    # Sort first, so the survivor of a duplicate pair is the entry the rest of the policy
    # scored highest rather than the one that happened to arrive first.
    return collapse_duplicates(sort_by_score(importance))


@dataclass(frozen=True, slots=True)
class Page:
    """One page of results, plus what the policy did to produce it.

    Three numbers travel with the page rather than being recomputed by each caller, and
    each answers a question a pager has to get right:

    * ``results`` -- the page itself.
    * ``held_back`` -- candidates the per-site cap skipped *while filling this page*. Page
      scoped, because they are reachable on a later one.
    * ``pages`` -- how many pages this pool can produce at this limit, walked rather than
      estimated. `ceil(reachable / limit)` looks right and is not: pages are allowed to be
      shorter than ``limit``, and on this corpus they are -- `github` fills eight on the
      first page and four on the rest -- so the estimate undercounts and the pager would
      stop a visitor early.
    """

    results: list[SearchHit]
    held_back: int
    pages: int


def take_page(
    pool: Sequence[SearchHit],
    *,
    offset: int = 0,
    limit: int | None = None,
    max_per_domain: int = DEFAULT_MAX_PER_DOMAIN,
) -> Page:
    """The `limit` results at `offset` from an ordered pool, capped per site *per page*.

    Returns the page and how many candidates the cap held back *while filling it* -- the
    latter because it is the honest version of the "N hidden by the per-site cap" count: it
    describes this page, and those candidates are still reachable on a later one.

    ## Why the cap is a property of a page and not of the pool

    It used to be applied once, to the whole candidate window, and that made paging
    impossible in exactly the case the cap exists for. Measured on this corpus: the
    query `github` matches 13 816 documents, the top 400 by BM25 came from **four**
    hosts, and a two-per-site cap over all 400 left **eight** results. Page two asked
    for offset 10 and got an empty list, while the response reported 13 816 matches and
    the page offered a page-per-hundred pager. Widening the window does not fix it --
    3 200 candidates took 603 ms and were still from nine hosts -- because the cap, not
    the window, was the binding constraint.

    Per page, the same policy means what it was written to mean: no site takes more
    than `max_per_domain` of the ten slots a visitor is looking at.

    ## How a page is built

    Repeatedly: each pass over what is left of the pool accepts up to `limit` candidates,
    at most `max_per_domain` per host, and everything it did not accept -- the overflow
    past `limit` *and* everything the cap held back -- carries over to the next pass. Page
    *k* is the k-th pass.

    Carrying the skipped candidates over is the part that makes this work rather than
    merely differ. A single forward pass that merely skipped them would spend page one's
    slack on hosts already on it and then leave page two with whatever happened to be
    left -- which is the same "page two is short or empty" failure in a new place. Each
    pass instead starts from the same pool position and a fresh count per host, so every
    candidate is eventually accepted, on the earliest page that can take it.

    Two properties follow, and both are asserted in the API tests: no result appears on
    two pages, and ``ceil(len(pool) / limit)`` pages can all be answered -- a pass accepts
    at least one candidate while any remain, and a candidate is only ever set aside for a
    later page, never dropped. (The last page may hold fewer than `limit`; that is the end
    of the pool, not a cap.)

    ``limit=None`` switches to the old whole-pool behaviour (cap applied across the
    pool), which is what a caller that wants "everything, diversify it" should ask for.
    """
    if max_per_domain < 1:
        raise ValueError("max_per_domain must be at least 1")
    if offset < 0:
        raise ValueError("offset must not be negative")
    if limit is None:
        whole = enforce_domain_diversity(pool, max_per_domain=max_per_domain)
        return Page(results=whole, held_back=len(pool) - len(whole), pages=1)
    if limit < 1:
        raise ValueError("limit must be at least 1")

    wanted_page = offset // limit
    page: list[SearchHit] = []
    held_back = 0
    remaining = list(pool)
    passes = 0

    # Walks every page rather than only up to the one asked for, because the count of them
    # is a number the response has to carry (see `Page.pages`) and the walk is one pass over
    # what is left per page. The pool is bounded by the candidate window -- 400 at most -- so
    # this is a few thousand dictionary lookups, not a scan that grows with the index. A
    # pass accepts at least one candidate while any remain, which is also what makes the
    # loop terminate.
    while remaining:
        counts: dict[str, int] = {}
        kept: list[SearchHit] = []
        carried: list[SearchHit] = []

        for hit in remaining:
            host = _host(hit.url)
            if len(kept) < limit and counts.get(host, 0) < max_per_domain:
                counts[host] = counts.get(host, 0) + 1
                kept.append(hit)
            else:
                carried.append(hit)
                # Counted only for the page being returned: a skip while advancing to it
                # is not something the caller can see, and reporting it would overstate
                # what the cap cost on this page.
                if passes == wanted_page and len(kept) < limit:
                    held_back += 1

        if passes == wanted_page:
            page = kept
        remaining = carried
        passes += 1

    return Page(results=page, held_back=held_back, pages=passes)


def rank(
    hits: Sequence[SearchHit],
    *,
    now: int,
    query: str = "",
    half_life_days: float = DEFAULT_HALF_LIFE_DAYS,
    max_per_domain: int = DEFAULT_MAX_PER_DOMAIN,
    authority_weight: float = DEFAULT_AUTHORITY_WEIGHT,
    host_authority_weight: float = DEFAULT_HOST_AUTHORITY_WEIGHT,
    title_weight: float = DEFAULT_TITLE_WEIGHT,
    legal_penalty: float = DEFAULT_LEGAL_PENALTY,
    locale_penalty: float = DEFAULT_LOCALE_PENALTY,
    search_view_penalty: float = DEFAULT_SEARCH_VIEW_PENALTY,
    depth_decay: float = DEFAULT_DEPTH_DECAY,
    site_root_weight: float = DEFAULT_SITE_ROOT_WEIGHT,
    site_host_weight: float = DEFAULT_SITE_HOST_WEIGHT,
) -> tuple[list[SearchHit], int]:
    """Runs the whole policy over one window, returning the capped pool and what it dropped.

    Whole-pool capping: `take_page` is the paging path, this is the "diversify everything
    you were given" one. Both are here because they answer different questions, and it is
    the cap's scope -- not the ordering -- that differs between them.
    """
    unique = ordered_pool(
        hits,
        now=now,
        query=query,
        half_life_days=half_life_days,
        authority_weight=authority_weight,
        host_authority_weight=host_authority_weight,
        title_weight=title_weight,
        legal_penalty=legal_penalty,
        locale_penalty=locale_penalty,
        search_view_penalty=search_view_penalty,
        depth_decay=depth_decay,
        site_root_weight=site_root_weight,
        site_host_weight=site_host_weight,
    )
    kept = enforce_domain_diversity(unique, max_per_domain=max_per_domain)
    # Counted against the candidates as they arrived, so a page dropped as a duplicate and
    # a page dropped by the domain cap are both visible in one number.
    return kept, len(hits) - len(kept)


def _normalise(text: str) -> str:
    return " ".join(text.lower().split())


def collapse_duplicates(hits: Sequence[SearchHit]) -> list[SearchHit]:
    """Drops a page that is already in the list under a different URL.

    A crawl finds the same document at more than one address -- a front page and its
    `/webhp` equivalent, a page and the redirect that also serves it -- and two entries
    for one document is the most obvious way a results page looks wrong: identical
    titles, one after the other, at the top.

    The rule is deliberately narrow, and needed to be: same host *and* same title *and*
    the same snippet text. A repeated title on its own means nothing here -- every page
    of a documentation site can be titled after the project -- and a shared snippet is
    what says two URLs are serving the same words.
    """
    seen: set[tuple[str, str, str]] = set()
    kept: list[SearchHit] = []

    for hit in hits:
        key = (_host(hit.url), _normalise(hit.title), hit.snippet)
        if key in seen:
            continue
        seen.add(key)
        kept.append(hit)

    return kept


def enforce_domain_diversity(
    hits: Iterable[SearchHit],
    *,
    max_per_domain: int = DEFAULT_MAX_PER_DOMAIN,
) -> list[SearchHit]:
    """Caps how many results from any one host appear in the output.

    Preserves input order, which is the ranking order. Results beyond the cap are
    dropped rather than moved further down: the point is to stop one site from
    monopolising the first page, and a demoted-but-still-present result would not
    achieve that.
    """
    if max_per_domain < 1:
        raise ValueError("max_per_domain must be at least 1")

    counts: dict[str, int] = {}
    kept: list[SearchHit] = []

    for hit in hits:
        host = _host(hit.url)
        seen = counts.get(host, 0)
        if seen >= max_per_domain:
            continue
        counts[host] = seen + 1
        kept.append(hit)

    return kept
