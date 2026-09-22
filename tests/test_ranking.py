"""Tests for the ranking post-processing passes."""

from __future__ import annotations

import pytest

from api.models.responses import SearchHit
from api.services.ranking import (
    apply_freshness_boost,
    apply_importance,
    collapse_duplicates,
    enforce_domain_diversity,
    host_label,
    is_search_view,
    query_terms,
    rank,
    sort_by_score,
    take_page,
    title_match,
    url_quality,
)

SECONDS_PER_DAY = 86_400
NOW = 2_000_000_000


def hit(
    url: str = "https://a.example/1",
    score: float = 10.0,
    age_days: float = 0.0,
    *,
    title: str = "title",
    authority: float = 0.0,
    host_authority: float = 0.0,
    snippet: str = "snippet",
) -> SearchHit:
    return SearchHit(
        url=url,
        title=title,
        snippet=snippet,
        fetched_at=int(NOW - age_days * SECONDS_PER_DAY),
        score=score,
        authority=authority,
        host_authority=host_authority,
    )


# ---------- navigational queries ----------


def test_a_host_is_named_by_the_label_that_is_its_name() -> None:
    # The mapping this pins, because every case here appears in the corpus: a www prefix
    # is a hostname convention, a subdomain belongs to the site it documents, and a
    # two-part suffix must not be mistaken for the name.
    assert host_label("https://www.python.org/downloads/") == "python"
    assert host_label("https://python.org/") == "python"
    assert host_label("https://docs.github.com/en") == "github"
    assert host_label("https://kubernetes.io/docs/") == "kubernetes"
    assert host_label("https://www.blender.org/") == "blender"
    assert host_label("https://example.co.uk/page") == "example"
    assert host_label("https://localhost/") == "localhost"


def test_a_query_that_names_a_site_lifts_that_sites_front_page() -> None:
    # The bug this pins: on this corpus, `python` ranked python.org/downloads first and
    # the front page did not appear in the top five at all, because a download page
    # repeats the word more often than the page that *is* the site does. Text relevance
    # cannot answer a navigation; the site's own name can.
    hits = [
        hit("https://www.python.org/downloads/", 9.70, title="Download Python | Python.org"),
        hit("https://www.python.org/", 8.10, title="Welcome to Python.org"),
    ]

    ranked, _ = rank(hits, now=NOW, query="python", max_per_domain=10)

    assert ranked[0].url == "https://www.python.org/"


def test_naming_a_site_lifts_its_pages_above_another_sites_mentions() -> None:
    hits = [
        hit("https://blog.example/why-we-use-docker", 9.0, title="Why we use Docker"),
        hit("https://www.docker.com/pricing/", 8.0, title="Pricing | Docker"),
    ]

    ranked, _ = rank(hits, now=NOW, query="docker", max_per_domain=10)

    assert ranked[0].url == "https://www.docker.com/pricing/"


def test_a_navigation_puts_the_named_site_first_even_when_its_own_root_matches_least() -> None:
    # Measured on the corpus, and the reason the root weight is as large as it is:
    # `github.com/` scores 3.31 for `github` while `docs.github.com/en` scores 9.31, so a
    # bonus that merely nudges leaves the documentation index above the product's home
    # page for the query that is its name.
    hits = [
        hit("https://docs.github.com/en", 9.31, title="GitHub Docs", host_authority=1.0),
        hit("https://github.com/", 3.31, title="GitHub · Build software better"),
    ]

    ranked, _ = rank(hits, now=NOW, query="github", max_per_domain=10)

    assert ranked[0].url == "https://github.com/"


def test_naming_a_site_and_a_subject_is_not_a_navigation() -> None:
    # `python docs` asks for documentation, not for the front page: the root bonus must
    # not fire just because one of the terms happens to be a site's name.
    hits = [
        hit("https://docs.python.org/3/", 9.0, title="Python 3 documentation"),
        hit("https://www.python.org/", 8.5, title="Welcome to Python.org"),
    ]

    ranked, _ = rank(hits, now=NOW, query="python docs", max_per_domain=10)

    assert ranked[0].url == "https://docs.python.org/3/"


def test_a_hostname_written_out_is_still_one_navigation() -> None:
    # `www`, `org` and friends are parts of an address, not subject words.
    hits = [
        hit("https://www.python.org/downloads/", 9.7, title="Download Python"),
        hit("https://www.python.org/", 4.0, title="Welcome to Python.org"),
    ]

    ranked, _ = rank(hits, now=NOW, query="www.python.org", max_per_domain=10)

    assert ranked[0].url == "https://www.python.org/"


def test_a_query_that_names_no_site_lifts_no_front_page() -> None:
    # The bonus has to be triggered by a *name*, not by the shape of a URL, so a root
    # page whose host has nothing to do with the query must not be lifted over a better
    # match just for being a root.
    hits = [
        hit("https://blog.example/why-we-use-docker", 10.0, title="Why we use Docker"),
        hit("https://docker.example/", 8.5, title="Docker hosting"),
    ]

    ranked, _ = rank(hits, now=NOW, query="container orchestration", max_per_domain=10)

    assert ranked[0].url == "https://blog.example/why-we-use-docker"


# ---------- duplicate collapsing ----------


def test_the_same_page_under_two_urls_is_shown_once() -> None:
    # Seen on the live corpus: `https://www.google.com/` and
    # `https://www.google.com/webhp?tab=ww` both came back, with the same title and the
    # same text, one after the other at the top of the page.
    hits = [
        hit("https://www.google.com/", 12.0, title="Google", snippet="Gmail Bilder"),
        hit("https://www.google.com/webhp?tab=ww", 10.8, title="Google", snippet="Gmail Bilder"),
    ]

    kept = collapse_duplicates(hits)

    assert [entry.url for entry in kept] == ["https://www.google.com/"]


def test_a_shared_title_alone_does_not_collapse_pages() -> None:
    # Every page of a documentation site can be titled after the project, and those are
    # different pages: the same title with different text must both survive.
    hits = [
        hit(
            "https://kubernetes.io/docs/home/",
            18.0,
            title="Kubernetes",
            snippet="production-grade",
        ),
        hit("https://kubernetes.io/docs/concepts/", 16.0, title="Kubernetes", snippet="concepts"),
    ]

    assert len(collapse_duplicates(hits)) == 2


def test_the_same_words_on_two_different_hosts_are_two_pages() -> None:
    hits = [
        hit("https://a.example/", 10.0, title="Home", snippet="Welcome"),
        hit("https://b.example/", 9.0, title="Home", snippet="Welcome"),
    ]

    assert len(collapse_duplicates(hits)) == 2


def test_a_duplicate_is_counted_as_suppressed() -> None:
    hits = [
        hit("https://www.google.com/", 12.0, title="Google", snippet="Gmail", age_days=0),
        hit("https://www.google.com/webhp", 10.0, title="Google", snippet="Gmail", age_days=0),
    ]

    ranked, suppressed = rank(hits, now=NOW, query="google", max_per_domain=10)

    assert len(ranked) == 1
    assert suppressed == 1


def test_a_document_fetched_now_keeps_its_score() -> None:
    boosted = apply_freshness_boost([hit()], now=NOW, half_life_days=30)
    assert boosted[0].score == pytest.approx(10.0)


def test_one_half_life_halves_the_score() -> None:
    boosted = apply_freshness_boost([hit(age_days=30)], now=NOW, half_life_days=30)
    assert boosted[0].score == pytest.approx(5.0)


def test_two_half_lives_quarter_the_score() -> None:
    boosted = apply_freshness_boost([hit(age_days=60)], now=NOW, half_life_days=30)
    assert boosted[0].score == pytest.approx(2.5)


def test_a_future_timestamp_cannot_outrank_a_fresh_document() -> None:
    # Clock skew or a page dated in the future must not become a ranking hack.
    future = apply_freshness_boost([hit(age_days=-365)], now=NOW, half_life_days=30)
    fresh = apply_freshness_boost([hit(age_days=0)], now=NOW, half_life_days=30)
    assert future[0].score == pytest.approx(fresh[0].score)


def test_boosting_does_not_mutate_the_input() -> None:
    original = hit(age_days=30)
    apply_freshness_boost([original], now=NOW, half_life_days=30)
    assert original.score == 10.0


def test_a_non_positive_half_life_is_rejected() -> None:
    with pytest.raises(ValueError, match="half_life_days"):
        apply_freshness_boost([hit()], now=NOW, half_life_days=0)


def test_domain_diversity_caps_results_per_host() -> None:
    hits = [hit(url=f"https://same.example/{n}") for n in range(5)]

    kept = enforce_domain_diversity(hits, max_per_domain=2)

    assert [h.url for h in kept] == ["https://same.example/0", "https://same.example/1"]


def test_domain_diversity_preserves_ranking_order_across_hosts() -> None:
    hits = [
        hit(url="https://a.example/1", score=9.0),
        hit(url="https://b.example/1", score=8.0),
        hit(url="https://a.example/2", score=7.0),
        hit(url="https://c.example/1", score=6.0),
    ]

    kept = enforce_domain_diversity(hits, max_per_domain=2)

    assert [h.score for h in kept] == [9.0, 8.0, 7.0, 6.0]


def test_www_and_bare_hosts_are_treated_as_different_domains() -> None:
    # Documented behaviour rather than an accident: collapsing these would need
    # a public-suffix list, and getting that subtly wrong is worse than not
    # doing it.
    hits = [hit(url="https://www.example.com/1"), hit(url="https://example.com/1")]

    kept = enforce_domain_diversity(hits, max_per_domain=1)

    assert len(kept) == 2


def test_domain_diversity_rejects_a_zero_cap() -> None:
    with pytest.raises(ValueError, match="max_per_domain"):
        enforce_domain_diversity([hit()], max_per_domain=0)


def test_sorting_is_stable_so_equal_scores_keep_the_engines_order() -> None:
    # Once the freshness multiplier underflows to zero, every score on the page
    # is equal and the engine's own order is all that is left. An unstable sort
    # would reshuffle that page on every request.
    hits = [hit(url=f"https://a.example/{n}", score=0.0) for n in range(8)]

    assert [h.url for h in sort_by_score(hits)] == [h.url for h in hits]


def test_ranking_reorders_a_page_the_engine_ranked_by_raw_score() -> None:
    # Distinct hosts so that the diversity cap cannot be what reorders them: this
    # test is about the sort.
    stale = [hit(url=f"https://old{n}.example/1", score=10.0 - n, age_days=3_000) for n in range(3)]
    fresh = hit(url="https://new.example/1", score=1.0, age_days=0)

    ranked, suppressed = rank([*stale, fresh], now=NOW, half_life_days=30)

    # The boost is worthless without the sort that follows it, so this is the
    # assertion that the whole policy is actually in force.
    assert ranked[0].url == "https://new.example/1"
    assert suppressed == 0


def test_ranking_reports_what_the_diversity_cap_dropped() -> None:
    # Distinct text, because the fixture has to be four different pages: four entries
    # that read identically are the same document under four URLs, and those are
    # collapsed before the cap ever sees them (see `collapse_duplicates`).
    hits = [
        hit(url=f"https://same.example/{n}", score=10.0 - n, snippet=f"page {n}") for n in range(4)
    ]

    ranked, suppressed = rank(hits, now=NOW, query="q", max_per_domain=2)

    assert len(ranked) == 2
    assert suppressed == 2


def test_ranking_keeps_the_page_when_the_boost_is_disabled_by_a_half_life() -> None:
    # A very long half life is the configuration that means "do not really boost"
    # rather than a special case in the code.
    hits = [hit(url=f"https://a{n}.example/1", score=10.0 - n, age_days=n * 10) for n in range(3)]

    ranked, _ = rank(hits, now=NOW, half_life_days=100_000)

    assert [h.url for h in ranked] == [h.url for h in hits]


# --- the importance layer -------------------------------------------------------


def test_the_front_page_beats_the_privacy_policy() -> None:
    # The failure this layer was added for. Both pages match the query; only one of
    # them is what someone typing a company's name meant. The policy can even match
    # the name *more often* and still must lose.
    home = hit("https://example.com/", 4.0, title="Example")
    policy = hit("https://example.com/legal/privacy-policy", 6.0, title="Example privacy policy")
    terms = query_terms("example")

    ranked = sort_by_score(apply_importance([policy, home], query="example"))

    assert ranked[0].url == "https://example.com/"
    # ...and both facts are visible in the numbers rather than only in the order.
    assert url_quality(home.url) > url_quality(policy.url)
    assert title_match(home.title, terms) > title_match(policy.title, terms)


def test_a_page_about_the_subject_beats_a_legal_page_on_the_same_host() -> None:
    # The second half of the same request: an article *about* the thing should also
    # come before the site's terms of service.
    article = hit("https://news.example/example-company", 5.0, title="Example Inc, explained")
    terms = hit("https://example.com/terms", 5.5, title="Terms of service")

    ranked = sort_by_score(apply_importance([terms, article], query="example"))

    assert ranked[0].url == "https://news.example/example-company"


def test_authority_lifts_a_page_past_a_better_textual_match() -> None:
    # What authority is for: a slight relevance gap should not hand the top slot to
    # a page nothing links to.
    hub = hit("https://example.com/docs", 4.0, title="Docs", authority=0.9)
    leaf = hit("https://other.example/page", 4.5, title="Page")

    ranked = sort_by_score(apply_importance([leaf, hub], query="docs"))

    assert ranked[0].url == "https://example.com/docs"


def test_a_strong_relevance_lead_is_not_overturned_by_authority() -> None:
    # The other direction has to hold too, or ranking becomes "whoever is biggest
    # wins" and relevance stops meaning anything.
    hub = hit("https://example.com/unrelated", 1.0, title="Unrelated", authority=1.0)
    leaf = hit("https://other.example/exact", 30.0, title="Exact")

    ranked = sort_by_score(apply_importance([leaf, hub], query="exact"))

    assert ranked[0].url == "https://other.example/exact"


def test_page_and_host_authority_are_separate_signals() -> None:
    # They are two fields rather than one blended figure so that the policy can
    # weigh them differently, and so that a real "/" page can be counted once for
    # being linked to and once for sitting on a site other sites link to.
    page = hit("https://linked.example/a", 5.0, title="A", authority=0.9)
    host = hit("https://popular.example/a", 5.0, title="A", host_authority=0.9)
    neither = hit("https://obscure.example/a", 5.0, title="A")

    ranked = sort_by_score(apply_importance([neither, host, page], query="a"))
    scores = {h.url: h.score for h in ranked}

    assert scores["https://linked.example/a"] > scores["https://popular.example/a"]
    assert scores["https://popular.example/a"] > scores["https://obscure.example/a"]
    # A page that is both is counted twice, which is the point of keeping them apart.
    both = hit("https://linked.example/b", 5.0, title="A", authority=0.9, host_authority=0.9)
    assert apply_importance([both], query="a")[0].score > scores["https://linked.example/a"]


def test_legal_pages_are_penalised_across_the_spellings_they_use() -> None:
    for url in [
        "https://example.com/privacy",
        "https://example.com/privacy-policy",
        "https://example.com/legal/terms-of-service",
        "https://example.com/en/terms",
        "https://example.com/policies/cookies",
        "https://example.com/about/imprint",
        "https://example.com/disclaimer.html",
    ]:
        assert url_quality(url) < 0.4, url


def test_ordinary_pages_are_not_penalised_as_legal_furniture() -> None:
    for url in [
        "https://example.com/",
        "https://example.com/products",
        "https://example.com/blog/2026/why-policies-matter",
        "https://example.com/policyholders-guide",
    ]:
        assert url_quality(url) >= 0.8, url


def test_locale_variants_of_one_page_are_demoted() -> None:
    # google.com's sitemaps list every language, so the same page arrives as forty
    # URLs. Without this an English query ranks several of them.
    assert url_quality("https://workspace.example/intl/ja/products/forms") < url_quality(
        "https://workspace.example/products/forms"
    )
    assert url_quality("https://example.com/en-US/docs") < url_quality("https://example.com/docs")


def test_a_bare_two_letter_path_segment_is_not_read_as_a_language() -> None:
    # `/go/to` and `/my/page` are ordinary paths, and treating them as locales
    # would quietly demote them.
    assert url_quality("https://example.com/go/to") == url_quality("https://example.com/other/to")


def test_deeper_pages_are_demoted_but_never_vanishingly_so() -> None:
    shallow = url_quality("https://example.com/a/b")
    deep = url_quality("https://example.com/a/b/c/d/e/f/g/h/i/j")

    assert deep < shallow
    # The floor: a deep page is still a page, and a corpus of deep pages must not
    # produce a page of zeroes.
    assert deep > 0.5


def test_the_root_page_is_promoted() -> None:
    assert url_quality("https://example.com/") > url_quality("https://example.com/a")


def test_title_match_is_relative_to_the_title_not_the_query() -> None:
    assert title_match("Google", ("google",)) == 1.0
    # Contains the term, but the title is mostly about something else.
    assert title_match("Google privacy policy", ("google",)) < 0.5
    assert title_match("Google Cloud Platform", ("google", "cloud")) > 0.5
    assert title_match("Something else", ("google",)) == 0.0
    assert title_match("", ("google",)) == 0.0
    # No query, no credit -- and no division by zero.
    assert title_match("Google", ()) == 0.0


def test_query_terms_ignore_punctuation_and_case() -> None:
    assert query_terms("Rust, Ownership & Borrowing!") == ("rust", "ownership", "borrowing")
    assert query_terms("") == ()
    assert query_terms("???") == ()


def test_a_zero_penalty_is_rejected_rather_than_erasing_every_page() -> None:
    with pytest.raises(ValueError, match="penalties"):
        url_quality("https://example.com/legal/terms", legal_penalty=0)


def test_a_site_search_page_is_recognised_in_both_of_its_spellings() -> None:
    # The path form: the site puts its search under a segment.
    assert is_search_view("https://www.apple.com/us/search")
    assert is_search_view("https://www.chess.com/clubs/search")
    assert is_search_view("https://example.com/search/advanced")
    # The query form: the search *is* the query, and the path is the site's root.
    assert is_search_view("https://home.cern/?s=")
    assert is_search_view("https://www.nasa.gov/?search=Artemis")
    assert is_search_view("https://example.com/x?q=rust&page=2")
    # And these are ordinary pages that merely have a query.
    assert not is_search_view("https://example.com/docs?version=3")
    assert not is_search_view("https://www.biathlonworld.com/results?EventType=BTSWRLCP")
    assert not is_search_view("https://example.com/searching-for-meaning")
    assert not is_search_view("https://example.com/")


def test_a_site_search_page_ranks_below_the_page_it_searched_for() -> None:
    # The failure this exists for, measured rather than asserted by intuition: CERN's
    # empty search box came up second for a topic query. Its text is the site's own
    # furniture, so it must lose to a page that actually discusses the subject.
    assert url_quality("https://home.cern/?s=") < url_quality(
        "https://home.cern/how-accelerator-works/"
    )
    assert url_quality("https://www.nasa.gov/?search=Artemis") < url_quality(
        "https://www.nasa.gov/"
    )


def test_importance_does_not_mutate_the_input_and_is_off_at_zero_weight() -> None:
    original = hit("https://example.com/", 5.0, title="Example", authority=1.0)
    unchanged = apply_importance(
        [original],
        query="example",
        authority_weight=0,
        host_authority_weight=0,
        title_weight=0,
        # Every signal off, including the navigational one: a query of "example" against
        # `example.com` is exactly a navigation, so leaving that weight at its default
        # would make this test measure the new bonus instead of the URL prior.
        site_root_weight=0,
        site_host_weight=0,
    )

    # With the weights at zero only the URL prior is left, which is bounded and
    # observable rather than a hidden default.
    assert unchanged[0].score == pytest.approx(5.0 * url_quality("https://example.com/"))
    assert original.score == 5.0


def test_the_whole_policy_puts_the_right_page_first() -> None:
    # End to end through `rank`, which is what the router calls: the sort has to
    # come after the boosts or none of this is in force.
    hits = [
        hit(
            "https://example.com/legal/privacy", 6.5, title="Example privacy policy", authority=0.2
        ),
        hit("https://example.com/terms", 6.0, title="Terms of service"),
        hit("https://example.com/", 4.0, title="Example", authority=0.6),
        hit("https://news.example/example-inc", 4.2, title="Example Inc, explained"),
    ]

    ranked, _ = rank(hits, now=NOW, query="example", max_per_domain=10)

    assert [h.url for h in ranked][:2] == [
        "https://example.com/",
        "https://news.example/example-inc",
    ]
    # The legal pages are still present, just behind: a demotion is not a removal.
    assert len(ranked) == 4


def test_a_site_search_page_does_not_beat_a_page_that_discusses_the_subject() -> None:
    # The search view scores *better* textually, which is the whole point: it repeats
    # every term on the site's navigation. It must still lose to the article, and the
    # penalty has to survive the sort for that to happen.
    hits = [
        hit("https://home.cern/?s=", 9.5, title='Search Results for ""', authority=0.4),
        hit("https://home.cern/how-accelerator-works/", 6.0, title="How an accelerator works"),
    ]

    ranked, _ = rank(hits, now=NOW, query="accelerator works", max_per_domain=10)

    assert [h.url for h in ranked] == [
        "https://home.cern/how-accelerator-works/",
        "https://home.cern/?s=",
    ]


# ---------- the page walk ----------


def pool(*specs: tuple[str, int]) -> list[SearchHit]:
    """An ordered pool from `(host, count)` pairs, scores descending in list order."""
    built: list[SearchHit] = []
    for host, count in specs:
        built.extend(
            hit(f"https://{host}/{index}", 10.0 - len(built) / 100) for index in range(count)
        )
    return built


def test_pages_are_disjoint_and_each_page_is_capped_independently() -> None:
    # Two hosts, six results each, pages of four. Per page the cap allows two from each
    # host, so the first page is the two best of both -- and the third and fourth of each
    # host are not lost, they are the next page.
    ordered = pool(("one.example", 6), ("two.example", 6))

    first = take_page(ordered, offset=0, limit=4, max_per_domain=2).results
    second = take_page(ordered, offset=4, limit=4, max_per_domain=2).results
    third = take_page(ordered, offset=8, limit=4, max_per_domain=2).results

    assert [h.url for h in first] == [
        "https://one.example/0",
        "https://one.example/1",
        "https://two.example/0",
        "https://two.example/1",
    ]
    assert [h.url for h in second] == [
        "https://one.example/2",
        "https://one.example/3",
        "https://two.example/2",
        "https://two.example/3",
    ]
    assert [h.url for h in third] == [
        "https://one.example/4",
        "https://one.example/5",
        "https://two.example/4",
        "https://two.example/5",
    ]
    every = [h.url for h in first + second + third]
    assert len(every) == len(set(every)), "a result must not appear on two pages"
    # And the walk reports that there are exactly these three, rather than the three a
    # division would guess: two hosts of six in pages of four is 12 / 4 = 3, which happens
    # to agree here and does not in general (a page may hold fewer than `limit`).
    assert take_page(ordered, limit=4, max_per_domain=2).pages == 3


def test_the_reported_page_count_follows_a_capped_walk_not_a_division() -> None:
    # Nine candidates from one host, pages of three, two per host per page: each page holds
    # two, so there are five pages and not the three that `ceil(9 / 3)` suggests. Asserting
    # this is how the pager stops at the last page that actually has results.
    ordered = pool(("only.example", 9))

    page = take_page(ordered, limit=3, max_per_domain=2)

    assert page.pages == 5
    # Addressed by page, not by result count: page *k* starts at `(k - 1) * limit`, which is
    # the fifth page here. A caller stepping by `limit` would land between pages once the
    # pages are shorter than `limit` -- which is why the response carries `pages` and why
    # the client enumerates pages rather than adding up results.
    last = take_page(ordered, offset=4 * 3, limit=3, max_per_domain=2)
    assert len(last.results) == 1, "the last page holds the ninth candidate"


def test_every_page_a_pool_can_fill_is_reachable() -> None:
    # The property the API's `reachable` count promises: `ceil(pool / limit)` pages, none
    # of them empty. Asserted on the shape that used to break it -- one host owning the
    # whole pool, where the cap leaves two per page.
    ordered = pool(("only.example", 9))

    for page in range(3):
        got = take_page(ordered, offset=page * 3, limit=3, max_per_domain=2).results
        assert len(got) == 2, f"page {page + 1} still holds its two results"
        assert all(h.url.startswith("https://only.example/") for h in got)


def test_a_page_past_the_pool_is_empty_rather_than_a_scan() -> None:
    ordered = pool(("one.example", 3), ("two.example", 3))

    page = take_page(ordered, offset=40, limit=10, max_per_domain=2)

    assert page.results == []
    assert page.held_back == 0


def test_the_skips_the_cap_makes_on_this_page_are_counted() -> None:
    # One host with ten candidates and a page of four: the cap holds six back, and that
    # number is what the response reports as `capped_on_page`.
    ordered = pool(("only.example", 10))

    page = take_page(ordered, offset=0, limit=4, max_per_domain=2)

    assert len(page.results) == 2
    assert page.held_back == 8, "every other candidate on this page was held back by the cap"


def test_no_limit_applies_the_cap_across_the_whole_pool() -> None:
    # The old behaviour, kept for callers that want everything rather than a page.
    ordered = pool(("one.example", 5), ("two.example", 5))

    page = take_page(ordered, limit=None, max_per_domain=2)

    assert len(page.results) == 4
    assert page.held_back == 6
    assert page.pages == 1


def test_take_page_rejects_a_nonsense_limit_or_cap() -> None:
    ordered = pool(("one.example", 1))

    with pytest.raises(ValueError):
        take_page(ordered, limit=0)
    with pytest.raises(ValueError):
        take_page(ordered, offset=-1, limit=1)
    with pytest.raises(ValueError):
        take_page(ordered, limit=1, max_per_domain=0)
