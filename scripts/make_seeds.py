"""Builds a seed list for the crawler from the official sites' own sitemaps.

Why sitemaps rather than links: a crawl seeded with one homepage spends its whole
budget on whatever that page happens to link to, which is mostly the same handful
of navigation pages repeated across a site. A sitemap is the site's own list of
what it wants indexed, published in priority order. Reading it is the difference
between indexing a site's front door forty times and indexing forty of its pages.

The list is interleaved across sites rather than concatenated. With a
`--max-pages` budget, a list that is 900 google.com URLs followed by 100 of
everything else produces a corpus that is one site, however many seeds it was
given. Round-robin is what makes the budget land evenly.

Output is plain text, one URL per line, with `#` comments -- the format
`crawler --seed-file` reads.

    uv run python scripts/make_seeds.py                    # 1000 URLs
    uv run python scripts/make_seeds.py --limit 200 --out seeds/small.txt
"""

from __future__ import annotations

import argparse
import gzip
import logging
import re
import sys
import xml.etree.ElementTree as ET
from collections.abc import Iterable
from pathlib import Path
from urllib.parse import quote, urljoin, urlsplit, urlunsplit

import httpx

logger = logging.getLogger("make_seeds")

USER_AGENT = "MJLsearchBot/0.1 (+https://example.invalid/bot)"

# Wikis publish no sitemap we can read -- `robots.txt` names
# `/w/rest.php/site/v1/sitemap/0`, which answers 403 to this bot -- so they are
# discovered through the MediaWiki API instead. That is not a workaround: the API is
# the site's own machine-readable index, the same role a sitemap plays, and it answers
# the question a search engine actually has -- what is being read right now, and what
# is filed under a subject -- rather than "every article, alphabetically".
WIKI_APIS: dict[str, str] = {
    "en.wikipedia.org": "https://en.wikipedia.org/w/api.php",
    "de.wikipedia.org": "https://de.wikipedia.org/w/api.php",
    "www.wikidata.org": "https://www.wikidata.org/w/api.php",
    "en.wiktionary.org": "https://en.wiktionary.org/w/api.php",
}

# The most-viewed list is demand; these categories are breadth. A seed set built only
# from what is popular this hour is a seed set about the news, so the categories are
# deliberately broad subjects rather than anything this crawler is interested in.
#
# The list is long on purpose, and it is walked *into* rather than only read off: an
# encyclopedia's category is a shelf, and taking its first N members alphabetically
# gives N articles that share a first letter. Each subject is therefore opened up one
# level -- `Sports` is read through its subcategories, which is where `Biathlon` and
# the other niche subjects live -- because "find the niche things" is exactly the ask
# a flat, alphabetically-truncated shelf cannot satisfy.
WIKI_CATEGORIES: tuple[str, ...] = (
    # Sciences
    "Physics",
    "Chemistry",
    "Biology",
    "Mathematics",
    "Astronomy",
    "Geology",
    "Meteorology",
    "Computer_science",
    "Engineering",
    "Medicine",
    # Humanities and arts
    "History",
    "Philosophy",
    "Linguistics",
    "Literature",
    "Music",
    "Film",
    "Painting",
    "Architecture",
    "Photography",
    # Society, places and everyday subjects
    "Economics",
    "Politics",
    "Law",
    "Religion",
    "Education",
    "Geography",
    "Transport",
    "Military_history",
    "Agriculture",
    "Food_and_drink",
    "Clothing",
    "Games",
    "Sports",
    "Crafts",
    "Gardening",
    "Animals",
    "Plants",
)

# A subject is read through this many of its subcategories, taking this many members
# from each. Six subcategories of three is eighteen articles per subject -- wide enough
# that `Sports` yields football, biathlon and chess rather than three football clubs,
# and shallow enough that the request count stays in the low hundreds.
SUBCATEGORIES_PER_SUBJECT = 6
MEMBERS_PER_SUBCATEGORY = 3

# The sites the crawler is pointed at. Each is a site a real query names, chosen so
# that the corpus covers more than one vendor: a search engine that only knows one
# company's pages cannot be judged on whether it ranks them well.
DEFAULT_SITES: tuple[str, ...] = (
    # Developer platforms
    "https://github.com/",
    "https://docs.github.com/en",
    "https://gitlab.com/",
    "https://bitbucket.org/",
    "https://stackoverflow.com/",
    # The languages and runtimes this project is built from
    "https://www.rust-lang.org/",
    "https://doc.rust-lang.org/",
    "https://docs.python.org/3/",
    "https://www.python.org/",
    "https://nodejs.org/",
    "https://go.dev/",
    "https://www.typescriptlang.org/",
    "https://kotlinlang.org/",
    "https://www.php.net/",
    # Web platform and infrastructure
    "https://developer.mozilla.org/",
    "https://www.cloudflare.com/",
    "https://kubernetes.io/",
    "https://www.docker.com/",
    "https://nginx.org/",
    "https://www.postgresql.org/",
    "https://redis.io/",
    "https://www.sqlite.org/",
    "https://curl.se/",
    # Frameworks
    "https://react.dev/",
    "https://vuejs.org/",
    "https://fastapi.tiangolo.com/",
    "https://flask.palletsprojects.com/",
    "https://www.djangoproject.com/",
    "https://docs.djangoproject.com/en/stable/",
    # Vendors people search for by name
    "https://www.apple.com/",
    "https://www.microsoft.com/",
    "https://www.amazon.com/",
    "https://www.google.com/",
    "https://about.google/",
    "https://workspace.google.com/",
    "https://cloud.google.com/",
    # Reference works. An encyclopedia is one kind of answer among several rather than
    # the default one, which is why these sit alongside the sites a query might
    # actually name instead of replacing them.
    "https://en.wikipedia.org/wiki/Main_Page",
    "https://de.wikipedia.org/wiki/Wikipedia:Hauptseite",
    "https://www.wikidata.org/wiki/Wikidata:Main_Page",
    # A dictionary is the other half of "what is this niche thing": an encyclopedia
    # explains a subject, a lexicon explains the word someone actually typed, and for a
    # rare term it is often the only source that has an entry at all.
    "https://en.wiktionary.org/wiki/Wiktionary:Main_Page",
    "https://plato.stanford.edu/",
    "https://iep.utm.edu/",
    "https://www.britannica.com/",
    "https://archive.org/",
    "https://www.gutenberg.org/",
    # Video and other platforms: a video page's indexable text is its title and
    # description, which is exactly what a result needs to be useful.
    "https://www.youtube.com/",
    "https://www.imdb.com/",
    "https://dev.to/",
    "https://lobste.rs/",
    "https://news.ycombinator.com/",
    # Subject coverage that is not a vendor and not an encyclopedia: the point of these
    # is that a query about a ski race, a particle accelerator, a national park or an
    # open textbook has somewhere to land other than a Wikipedia article. Each one was
    # checked for a `robots.txt` that this bot may read and a published sitemap; sites
    # that answer 403/418 to a named bot are not here, because a seed that is refused
    # spends a request and returns nothing.
    "https://www.biathlonworld.com/",
    "https://www.fis-ski.com/",
    "https://home.cern/",
    "https://www.nasa.gov/",
    "https://www.esa.int/",
    "https://www.who.int/",
    "https://www.nps.gov/",
    "https://www.un.org/",
    "https://openstax.org/",
    "https://www.smithsonianmag.com/",
    "https://www.chess.com/",
    "https://mathworld.wolfram.com/",
    "https://www.merriam-webster.com/",
    # Question-and-answer sites, which are where the niche questions live: an answer
    # about a `find` invocation or a LaTeX error is not on the vendor's homepage.
    "https://unix.stackexchange.com/",
    "https://askubuntu.com/",
    "https://math.stackexchange.com/",
    "https://superuser.com/",
    "https://serverfault.com/",
    "https://physics.stackexchange.com/",
    # Academic and long-tail sources.
    "https://arxiv.org/",
    "https://www.gnu.org/",
    "https://www.kernel.org/",
    "https://llvm.org/",
    "https://www.lua.org/",
    "https://www.perl.org/",
    "https://elixir-lang.org/",
    "https://www.haskell.org/",
    "https://julialang.org/",
)

# Hosts that are separate sites with their own robots.txt, allowed explicitly.
DEFAULT_EXTRA_HOSTS: tuple[str, ...] = (
    "docs.github.com",
    "doc.rust-lang.org",
    "docs.python.org",
    "developer.mozilla.org",
    "docs.djangoproject.com",
    "cloud.google.com",
    "support.google.com",
    "policies.google.com",
    "en.wikipedia.org",
    "de.wikipedia.org",
    "www.wikidata.org",
    "plato.stanford.edu",
    "www.britannica.com",
)

# Extensions that are never a search result: a page is what gets indexed.
ASSET_SUFFIXES = (
    ".jpg",
    ".jpeg",
    ".png",
    ".gif",
    ".webp",
    ".avif",
    ".svg",
    ".ico",
    ".bmp",
    ".css",
    ".js",
    ".mjs",
    ".json",
    ".xml",
    ".txt",
    ".rss",
    ".atom",
    ".pdf",
    ".zip",
    ".gz",
    ".tar",
    ".tgz",
    ".bz2",
    ".xz",
    ".7z",
    ".rar",
    ".mp3",
    ".mp4",
    ".webm",
    ".ogg",
    ".wav",
    ".woff",
    ".woff2",
    ".ttf",
    ".eot",
    ".exe",
    ".dmg",
    ".msi",
    ".apk",
    ".deb",
    ".rpm",
    ".pkg",
    ".whl",
)

# A subdirectory crawl of a bigger site (docs.python.org/3/) is a lot of pages that
# would otherwise crowd out other sites, so each site contributes a bounded slice.
PER_SITE_FLOOR = 8
PER_SITE_CEILING = 60

# Wikipedias get a bigger slice than an ordinary site, and deliberately so: an
# encyclopedia is the only source in this list that covers every subject, so a corpus
# that can answer a question about biathlon at all will answer it from there. The bound
# still holds -- the wiki must not become the corpus -- but it is the breadth of the
# article namespace that is being bought, not one site's front-page depth.
WIKI_CEILING = 160

TRACKING_PARAM = re.compile(
    r"^(utm_|ref_|pd_rd_|pf_rd_|mc_|_ga$|fbclid$|gclid$|msclkid$|"
    r"ucbcb$|ucpp$|primeCampaignId$|ref$|referrer$)",
    re.IGNORECASE,
)


def canonical(url: str) -> str | None:
    """Strips a URL to the form that identifies one page, or rejects it.

    Tracking parameters are dropped and the fragment with them, because the same
    page under `?utm_source=newsletter` is the same page -- and a seed list that
    spends four of its thousand slots on it is a list with three dead entries.
    """
    try:
        parts = urlsplit(url.strip())
    except ValueError:
        return None

    if parts.scheme not in ("http", "https") or not parts.netloc:
        return None

    path = parts.path or "/"
    if path.lower().endswith(ASSET_SUFFIXES):
        return None

    kept = [
        pair
        for pair in parts.query.split("&")
        if pair and not TRACKING_PARAM.match(pair.split("=", 1)[0])
    ]
    return urlunsplit((parts.scheme, parts.netloc.lower(), path, "&".join(kept), ""))


def fetch(client: httpx.Client, url: str) -> bytes | None:
    try:
        response = client.get(url)
    # `ValueError` is not redundant with `httpx.HTTPError`: a relative or empty URL
    # reaches this through httpx's cookie handling into `urllib`, which raises a plain
    # `ValueError("unknown url type")`. That is a malformed *input* rather than a failed
    # request, and one site publishing a bad URL must not take the whole list down.
    except (httpx.HTTPError, ValueError) as error:
        logger.debug("unreachable %s (%s)", url, error)
        return None
    if response.status_code >= 400:
        logger.debug("%s -> %s", url, response.status_code)
        return None
    return response.content


def sitemaps_from_robots(client: httpx.Client, site: str) -> list[str]:
    """The `Sitemap:` URLs a site publishes, or the conventional guesses.

    `robots.txt` is the only place a sitemap's location is *specified*; the names
    `/sitemap.xml` and `/sitemap_index.xml` are conventions, tried only when the
    file names none, because a wrong guess costs one request and a missing sitemap
    costs the whole site.
    """
    parts = urlsplit(site)
    origin = f"{parts.scheme}://{parts.netloc}"
    body = fetch(client, f"{origin}/robots.txt")
    found: list[str] = []

    if body is not None:
        for line in body.decode("utf-8", "replace").splitlines():
            key, _, value = line.partition(":")
            if key.strip().lower() == "sitemap" and value.strip():
                # `Sitemap:` is specified as an absolute URL, but sites publish relative
                # paths (`/sitemap.xml`) and they are unambiguous: they are relative to
                # the `robots.txt` that names them, which is this origin.
                found.append(urljoin(f"{origin}/", value.strip()))

    if not found:
        for guess in (
            "/sitemap.xml",
            "/sitemap.xml.gz",
            "/sitemap_index.xml",
            "/sitemap-index.xml",
        ):
            if fetch(client, f"{origin}{guess}") is not None:
                found.append(f"{origin}{guess}")
                break

    return found


def locs(body: bytes) -> list[str]:
    """Every `<loc>` in a sitemap, namespace-agnostic.

    Parsed as XML rather than with a regex because sitemaps are required to be
    well-formed XML and are therefore allowed to be namespaced, wrapped, or
    indented; the namespace is stripped instead of guessed.
    """
    if body[:2] == b"\x1f\x8b":  # gzip magic: `sitemap.xml.gz` is a convention
        try:
            body = gzip.decompress(body)
        except OSError:
            return []

    try:
        root = ET.fromstring(body)
    except ET.ParseError as error:
        logger.debug("sitemap is not XML (%s)", error)
        return []

    return [
        element.text.strip()
        for element in root.iter()
        if element.tag.rsplit("}", 1)[-1] == "loc" and element.text and element.text.strip()
    ]


def wikipedia_pages(client: httpx.Client, api: str, origin: str, cap: int) -> list[str]:
    """Article URLs from a MediaWiki API: what is being read, then what is filed.

    Two queries rather than one, because they answer different questions and a seed set
    needs both: `mostviewed` is what people are looking for today (which is what makes
    this list current rather than a sample of the long tail), and the category members
    are the long tail itself -- the articles about a subject that nobody is reading this
    hour and that a search engine would still be wrong to lack.
    """
    pages: list[str] = []
    seen: set[str] = set()

    def collect(titles: Iterable[str]) -> None:
        for title in titles:
            if len(pages) >= cap:
                return
            if not isinstance(title, str) or not title.strip():
                continue
            page = canonical(f"{origin}/wiki/{quote(title.replace(' ', '_'), safe=":/()!',-")}")
            if page and page not in seen:
                seen.add(page)
                pages.append(page)

    def query(params: dict[str, str]) -> dict:
        try:
            response = client.get(api, params={**params, "format": "json"})
            if response.status_code >= 400:
                logger.debug("%s -> %s", api, response.status_code)
                return {}
            return response.json().get("query") or {}
        except (httpx.HTTPError, ValueError) as error:
            logger.debug("api %s failed (%s)", api, error)
            return {}

    # Article namespace only: the API's own list includes talk pages, portals and the
    # `Special:` namespace, none of which is a document anyone searches for.
    collect(
        entry.get("title")
        for entry in query(
            {
                "action": "query",
                "list": "mostviewed",
                "pvimlimit": str(min(cap, 40)),
            }
        ).get("mostviewed", [])
        if isinstance(entry, dict) and entry.get("ns") == 0
    )

    def titles(entries: Iterable[dict]) -> list[str]:
        return [
            entry["title"]
            for entry in entries
            if isinstance(entry, dict) and entry.get("ns") == 0 and entry.get("title")
        ]

    def members(category: str, kind: str) -> list[str]:
        return titles(
            query(
                {
                    "action": "query",
                    "list": "categorymembers",
                    "cmtitle": f"Category:{category}",
                    "cmlimit": str(SUBCATEGORIES_PER_SUBJECT),
                    "cmtype": kind,
                }
            ).get("categorymembers", [])
        )

    # One list per subject, then round-robin across the subjects. Collecting subject by
    # subject and truncating at the end would spend the whole budget on the first few
    # subjects in the tuple -- the sciences -- and leave `Sports` and `Crafts` unread,
    # which is the same failure the site quota in `interleave` exists to prevent, one
    # level down.
    per_subject: list[list[str]] = []
    for category in WIKI_CATEGORIES:
        subject: list[str] = []
        for subcategory in members(category, "subcat"):
            for title in members(subcategory, "page")[:MEMBERS_PER_SUBCATEGORY]:
                if title not in subject:
                    subject.append(title)
            if len(subject) >= SUBCATEGORIES_PER_SUBJECT * MEMBERS_PER_SUBCATEGORY:
                break
        # A category whose subcategories are themselves indexes (`Category:Sports` lists
        # countries, not sports) yields nothing here; its direct members are then the
        # only content it has, and they are better than an empty subject.
        if not subject:
            subject = members(category, "page")[:SUBCATEGORIES_PER_SUBJECT]
        if subject:
            per_subject.append(subject)

    for depth in range(SUBCATEGORIES_PER_SUBJECT * MEMBERS_PER_SUBCATEGORY):
        if len(pages) >= cap:
            break
        for subject in per_subject:
            if depth < len(subject):
                collect([subject[depth]])
            if len(pages) >= cap:
                break

    return pages


def site_pages(client: httpx.Client, site: str, allowed: set[str], cap: int) -> list[str]:
    """Canonical, same-site page URLs from one site's sitemaps, in published order."""
    parts = urlsplit(site)
    own = {parts.netloc.lower(), *allowed}
    pages: list[str] = []
    seen: set[str] = set()
    budget = max(cap, PER_SITE_FLOOR) * 3  # collect a surplus; the cap applies later

    for sitemap in sitemaps_from_robots(client, site):
        if len(pages) >= budget:
            break
        body = fetch(client, sitemap)
        if body is None:
            continue

        entries = locs(body)
        # A sitemap index lists sitemaps; its children are where the pages are. One
        # level is enough -- deeper nesting costs requests for pages a sibling
        # sitemap usually already lists.
        children = [entry for entry in entries if ".xml" in entry]
        if children and len(children) == len(entries):
            # A sitemap index lists sitemaps, so its own `<loc>`s are not pages.
            # They are replaced rather than appended to: keeping them would spend
            # the budget on `.xml` files that `canonical` then has to reject.
            entries = []
            for child in children[:20]:
                if len(pages) >= budget:
                    break
                child_body = fetch(client, child)
                if child_body is not None:
                    entries += locs(child_body)

        for entry in entries:
            page = canonical(entry)
            if page is None or page in seen:
                continue
            if urlsplit(page).netloc.lower() not in own:
                continue
            seen.add(page)
            pages.append(page)
            if len(pages) >= budget:
                break

    logger.info("%-42s %4d pages", urlsplit(site).netloc, len(pages))
    return pages


def interleave(per_site: list[list[str]], limit: int, share: int) -> list[str]:
    """Round-robins the sites, with each one capped at `share` pages.

    Two passes, and the quota is why. Round-robin alone is not enough: with twenty
    sites holding 180 pages each and eighteen holding fewer, a single pass spends
    the whole budget on the twenty and leaves the rest with nothing but their front
    page -- which is how the list this replaced ended up with twenty hosts taking 49
    URLs each and react.dev taking one. The first pass therefore gives every site at
    most `share` slots, and the second fills any remainder from the sites that have
    more, in the same even order.
    """
    out: list[str] = []
    seen: set[str] = set()
    deepest = max((len(pages) for pages in per_site), default=0)

    # Pass 1 walks the first `share` pages of every site; pass 2 takes the rest of
    # whatever any site still has left, for the case where the quota did not fill
    # the budget on its own.
    for depth in range(deepest):
        if depth == share:
            break
        for pages in per_site:
            if depth >= len(pages):
                continue
            page = pages[depth]
            if page in seen:
                continue
            seen.add(page)
            out.append(page)
            if len(out) >= limit:
                return out

    for depth in range(share, deepest):
        for pages in per_site:
            if depth >= len(pages):
                continue
            page = pages[depth]
            if page in seen:
                continue
            seen.add(page)
            out.append(page)
            if len(out) >= limit:
                return out

    return out


def build(sites: Iterable[str], allowed: set[str], limit: int) -> list[str]:
    per_site: list[list[str]] = []
    headers = {"user-agent": USER_AGENT, "accept": "text/html,application/xml;q=0.9,*/*;q=0.1"}

    with httpx.Client(headers=headers, follow_redirects=True, timeout=20.0, http2=False) as client:
        for site in sites:
            parts = urlsplit(site)
            api = WIKI_APIS.get(parts.netloc.lower())
            # One unreachable or malformed site contributes its front page and nothing
            # else, rather than aborting a build that has already spent minutes walking
            # the sitemaps of the sites that do work.
            try:
                if api:
                    pages = wikipedia_pages(
                        client, api, f"{parts.scheme}://{parts.netloc}", WIKI_CEILING
                    )
                else:
                    pages = site_pages(client, site, allowed, PER_SITE_CEILING)
            except (httpx.HTTPError, ValueError) as error:
                logger.warning("%-42s failed (%s)", parts.netloc, error)
                pages = []

            # The site's own entry point is always kept, and always first: it is the
            # page a navigational query for the site's name has to return, and a
            # sitemap that omits it (plenty do, listing only leaf pages) would leave
            # the front door to be discovered by following links instead. It is also
            # the *only* contribution of a site with no sitemap at all -- and
            # dropping those sites entirely was how the first version of this list
            # ended up without react.dev and sqlite.org in it.
            root = canonical(site)
            if root and root not in pages:
                pages.insert(0, root)

            per_site.append(pages)

    # An even share per site, so the budget covers the whole list of sites instead of
    # being consumed by whichever ones published the largest sitemaps.
    share = max(PER_SITE_FLOOR, -(-limit // max(len(per_site), 1)))
    logger.info("even share: %d pages per site", share)
    return interleave(per_site, limit, share)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--limit",
        type=int,
        default=3000,
        # More sources at the same per-site depth, rather than fewer sources at a
        # greater depth: the quota is `limit / sites`, so a list of fifty sources at
        # 1000 URLs would take the same twenty from each as before but leave half of
        # the topics untouched.
        help="maximum URLs to write",
    )
    parser.add_argument("--out", type=Path, default=Path("seeds/official.txt"))
    parser.add_argument("--site", action="append", default=[], help="extra site to include")
    parser.add_argument(
        "--allow-host", action="append", default=[], help="extra host a site may contribute"
    )
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args(argv)

    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(message)s",
    )
    # httpx logs every request at INFO, which would bury this script's own report in
    # a few thousand lines about sitemaps.
    logging.getLogger("httpx").setLevel(logging.DEBUG if args.verbose else logging.WARNING)

    sites = [*DEFAULT_SITES, *args.site]
    allowed = {*DEFAULT_EXTRA_HOSTS, *(host.lower() for host in args.allow_host)}

    urls = build(sites, allowed, args.limit)
    if not urls:
        logger.error("no URLs found -- network down?")
        return 1

    args.out.parent.mkdir(parents=True, exist_ok=True)
    header = (
        "# Generated by scripts/make_seeds.py from each site's own sitemap.\n"
        "# Regenerate with:  uv run python scripts/make_seeds.py\n"
        f"# {len(urls)} URLs across {len({urlsplit(url).netloc for url in urls})} hosts.\n"
        "# Used by:         ./start.sh  (and scripts/crawl_official.sh --seed-file)\n"
    )
    args.out.write_text(header + "\n".join(urls) + "\n", encoding="utf-8")

    hosts = sorted({urlsplit(url).netloc for url in urls})
    logger.info("wrote %d URLs across %d hosts to %s", len(urls), len(hosts), args.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
