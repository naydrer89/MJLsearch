# MJLsearch

A resource-conscious search engine: Rust for every CPU- and memory-critical path
(crawling, parsing, indexing, query execution), Python for the API surface, and a
direct PyO3 bridge between them with no network hop in between.

**Status: Phases 1-4 complete, plus a dashboard.** Crawling, indexing, query
execution, ranking post-processing and the analytics front end are implemented and
verified end to end against a real local crawl. Phase 5 (container build
validation) is the remaining work.

## Architecture

```
                    ┌────────────────────────────────────────────┐
                    │  crawler  (standalone, long-running)       │
   the web ────────▶│  tokio · reqwest · lol_html (streaming)    │
                    │  frontier + robots cache  (RocksDB)        │
                    │  seen URLs                (bloom filter)   │
                    └──────────────┬─────────────────────────────┘
                                   │  WARC + zstd  ──▶ data/warc
                                   │  documents    ──▶ data/spool/segment-*.pdoc
                                   ▼
                    ┌────────────────────────────────────────────┐
                    │  indexer  (standalone)                     │
                    │  drains sealed segments, commits in batches │
                    │  SOLE WRITER of the Tantivy index           │
                    └──────────────┬─────────────────────────────┘
                                   │  data/index/shard-000  (mmap)
                                   ▼
   HTTP ────▶┌───────────────────────────────┐   ┌────────────────────────────────┐
             │  api  (2-4 workers, read-only)│   │  dashboard  (1 worker, :5000)   │
             │  FastAPI ──▶ search_core (PyO3)│  │  same routers + web/ front end  │
             └───────────────────────────────┘   └────────────────────────────────┘
```

### Why the crawler does not write the index

Tantivy permits exactly one writer process per index. Ownership therefore has to
sit somewhere, and putting it in the indexer buys three things:

* a slow commit cannot stall the fetch loop;
* the crawler never links Tantivy, and the API process never links a TLS stack or
  an HTTP client — both dependency graphs get strictly smaller;
* reindexing is never reachable over HTTP, so the read-only API cannot become a
  write path by accident.

### The hand-off is a directory of sealed segments, not a shared database

The crawler writes length-prefixed `postcard` frames to `segment-N.open` and
rotates by renaming to `segment-N.pdoc`. The indexer reads only `.pdoc` files,
commits them, then deletes them.

A shared RocksDB was the obvious design and it does not work: RocksDB takes an
exclusive lock on its directory, so two processes cannot hold it, and the indexer
must *delete* what it consumes, which rules out a read-only handle as well. The
sealed-segment scheme has neither problem, is replayable after a crash, and
deletes the "where does the queue's encoding live" question along with it — nothing
outside `common::spool` needs to know the format.

The ordering that must not be relaxed: **commit before retiring**. Retiring first
would lose pages that were archived and parsed but never indexed, and nothing
downstream could detect the loss. The reverse order turns a crash into a replay.

## The memory strategy

Every structure that grows with the crawl lives on disk behind mmap, never on the
heap:

| Data | Where it lives | Why |
|---|---|---|
| Pending URLs | RocksDB, per-host FIFO keyed by `(host, seq)` | A heap queue grows unboundedly with the crawl |
| robots.txt rules | RocksDB, 24h TTL | A per-host map grows unboundedly at web scale |
| Seen URLs | Bloom filter, `data/urls.bloom` | ~114 MiB for 100M URLs at 1% FPR, against gigabytes for a `HashSet` |
| Fetched pages | WARC + zstd, on disk | Compressed archive, never held whole |
| Search index | Tantivy, mmap'd | Pages are shared by the OS across processes |
| Query cache | Bounded LRU (`moka`), fingerprint-stamped | Must never outlive a commit |
| Query analytics | Bounded ring buffer (512 entries) | The one API structure that grows with traffic |

The bloom filter and the query log are the only deliberately bounded heap
structures. Both are bounded rather than unbounded, and both have their bound
pinned by a test: 100M URLs at 1% false positives costs ~114 MiB with 7 hash
functions, and the analytics series stays a fixed 30 buckets no matter how long the
process runs.

## Repository layout

```
Cargo.toml               workspace: shared versions, release profile
pyproject.toml           uv workspace, FastAPI dependencies
crates/
  common/                Document + content_hash + the spool frame format
  crawler/               lib + bin; frontier, dedup, robots, fetcher, parser,
                         policy (URL canonicalisation + crawl scope), sitemaps, storage
  indexer/               lib + bin; drains the spool into Tantivy
  search-core/           lib (cdylib + rlib) + pyproject.toml for maturin
api/                     FastAPI gateway: routers, models, services, start-up chime
web/                     the dashboard: one HTML page, no build step, no framework
tests/                   pytest suite, extension stubbed where appropriate
scripts/                 build.sh, run_api.sh, run_searchui.sh, run_dashboard.sh,
                         make_seeds.py (sitemap -> seed list), crawl_official.sh
                         (sliced crawl + drain), audit_ui.py + audit_ui.js (UI audit)
start.sh                 one command: install, build, seed, start, verify
deploy/setup-server.sh   Ubuntu: apt, service user, build, systemd units, autostart
deploy/systemd/          the four unit templates (@APPDIR@/@SERVICE_USER@ placeholders)
deploy/mjlsearch.env.example  ports and knobs, installed to /etc/default/mjlsearch
seeds/official.txt       generated seed list (3000 URLs across 78 hosts)
Dockerfile.crawler       one image, both Rust binaries
Dockerfile.api           builds the abi3 wheel, installs on python:3.12-slim
docker-compose.yml       crawler + indexer + api + dashboard over a shared data/
data/                    gitignored: index/, warc/, spool/, rocksdb/, urls.bloom
```

## Requirements

* Rust 1.85+ (edition 2024), `uv`, and a C/C++ toolchain. `./start.sh` installs the
  first two itself if they are missing; the compiler it can only name.
* `clang` and `libclang` — `librocksdb-sys` compiles RocksDB from C++ source *and* runs
  rust-bindgen over its headers, so **the first build takes several minutes**.
  Subsequent builds are fast. `cmake` is in the lock file but never invoked: it is an
  optional builder of `aws-lc-sys`, which compiles with the C compiler here.
* Optional: any of `pw-play`, `paplay`, `aplay`, `ffplay` or `afplay` for the
  start-up chime. Without one, the server starts silently and says so in the log.

## Quick start

```bash
./start.sh                 # install, build, seed if the index is empty, start everything
```

That is the whole thing: it installs a missing Rust toolchain (rustup, minimal profile)
and `uv` into your home — the only prerequisite it cannot provide is a C/C++ compiler,
and it names the package when that is missing — builds the extension and both binaries,
generates a seed list from the official sites' own sitemaps if there is none, starts a
crawler in the background when the index is empty — looping rounds, so the growth page
stays live rather than flattening after one round — then brings up the API, the search
page on :5001 and the dashboard on :5000, detaching each one. Re-running it reuses
whatever is already answering its port and starts only what is missing. `--no-install`
turns the installation off, and on an Ubuntu server `sudo ./deploy/setup-server.sh`
does the same job with apt, a service account and systemd units.

The same thing by hand:

```bash
scripts/build.sh           # release extension + both Rust binaries
uv run python scripts/make_seeds.py --limit 1000   # -> seeds/official.txt

bash scripts/crawl_official.sh --loop 20          # crawl the list in rounds, draining as it goes
./target/release/indexer --once                   # or leave the indexer running

scripts/run_api.sh         # JSON API on :8000, 2 workers
scripts/run_searchui.sh    # search page on :5001
scripts/run_dashboard.sh   # dashboard on :5000, 1 worker
```

Check it:

```bash
curl -s localhost:8000/health
# {"status":"ok","extension_version":"0.1.0","detail":null,"index_open":true}

curl -s 'localhost:8000/search?q=borrowing&limit=3'
# {"query":"borrowing","took_ms":0.24,"total":1,...}
```

Then open <http://localhost:5001> to search, <http://localhost:5001/stats> for the
growth page, and <http://localhost:5000> for the analytics dashboard.

To start over, rebuild the corpus with `bash scripts/crawl_official.sh --fresh`.
Deleting `data/` by hand works too, but the frontier and the seen-set live in two
different files — `data/rocksdb` and `data/urls.bloom` — and removing only the first
leaves a crawler that believes every URL has already been fetched, exits zero, and
looks like a successful crawl of a site with nothing new. `--fresh` removes both.

### Managing the stack: manage.py

`manage.py` is a single stdlib-only file for servers and everyday use — it starts,
stops and inspects the whole stack, and it picks ports:

```bash
python3 manage.py start                    # build if needed, start api + ui + dashboard
python3 manage.py start --crawl            # ... plus looping crawl rounds
python3 manage.py start --host 0.0.0.0     # bind all interfaces (a public server)
python3 manage.py start --api-port 8100 --ui-port 5101   # explicit ports
python3 manage.py start --pick             # auto-select a free port when one is taken
python3 manage.py status                   # what is up, on which port, answering?
python3 manage.py status --json            # machine-readable
python3 manage.py logs api -f              # tail a log (api|ui|dashboard|crawl)
python3 manage.py stop                     # stop exactly what manage.py started
```

Everything it starts is detached into its own session and recorded in `logs/<name>.pid`,
so `stop` is exact — it kills only processes this tool started, never something that
merely holds a port — and a second `start` reuses services that already answer instead
of restarting them. On a server, `manage.py start --host 0.0.0.0 --crawl` after
`deploy/setup-server.sh` is the whole runtime story.

## The dashboard

One static page, no framework and no build step, polling `/analytics/overview`
every two seconds. A tool whose job is to watch the server should not need a
toolchain of its own.

It reports, and is honest about which is which:

* **Global** (files on disk written by other processes, so every server agrees):
  index document count and size, index revision, crawler progress, spool backlog.
* **Per-process**: query counters, latency percentiles, throughput series, top and
  slowest queries, RSS. uvicorn workers do not share memory, so these describe the
  process that answered — which is why the dashboard runs exactly one worker.

The crawler's progress is read from a small JSON file it rewrites every two
seconds, not queried from it: the crawler holds an exclusive lock on its RocksDB
directory while running, so asking is not an option.

### The start-up chime

`api/sound.py` synthesises a two-note chime with the standard library, caches it in
`data/sounds/startup.wav`, plays it through whichever player exists, and returns
immediately. Three properties are deliberate:

* it can never break start-up — no audio device is a log line, not an exception;
* it is detached, so it neither holds the server open nor dies with it;
* it plays **once per start**, not once per worker, which takes an explicit file
  lock: `uvicorn --workers 2` imports the app in both workers within milliseconds,
  and a plain check-then-write lets both through.

Set `SEARCH_STARTUP_SOUND=0` to silence it (the systemd units set it for you).

## Deployment (Ubuntu server)

```bash
sudo ./deploy/setup-server.sh
```

One command from a fresh server to four running services: it installs the apt
dependencies, picks the account to run as (`--user`, else the human who invoked sudo,
else the directory's owner), installs rustup and uv as that user, builds everything,
writes a seed list, renders the units from `deploy/systemd/*.service`, and enables and
starts them. Re-running it after `git pull` is the update path.

| Unit | Binds | Notes |
| --- | --- | --- |
| `mjlsearch-api` | 127.0.0.1:8000 | holds the index; `SIGINT` for a graceful stop |
| `mjlsearch-searchui` | **0.0.0.0**:5001 | the public surface, and the only unit bound to every interface |
| `mjlsearch-dashboard` | 127.0.0.1:5000 | operator console, loopback-only, no chime |
| `mjlsearch-crawl` | — | one round per run; `Restart=always` is the schedule, so growth continues |

The API and the console stay on loopback because neither has authentication in front of
it. Ports and ranking knobs live in `/etc/default/mjlsearch`, written once from
`deploy/mjlsearch.env.example` and never overwritten, so your edits survive an update.

## Verification

```bash
cargo build --workspace
cargo test --workspace                          # 244 tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

uv run pytest                                   # 195 tests
uv run ruff check . && uv run ruff format --check .

uv run python scripts/audit_ui.py --url http://127.0.0.1:5001/stats --query ''
```

Verified end to end against a real crawl: tens of thousands of documents across 79
hosts — the official sites, the wikis read through their own API, and the niche
sources (IU Biathlon, CERN, NASA, ESA, WHO, NPS, OpenStax, Merriam-Webster),
queried over HTTP with BM25 scores, `<b>`-marked snippets, a cache hit on the repeat
query, a 400 with Tantivy's own message for a malformed query, and a per-minute growth
timeline that the search page and `/stats` both read from the same derived series.
The UI audit reports zero findings at 320, 390, 834, 1440 and 1920 px on the landing
page, results, page two, no results, a rejected query, `/stats` and the dashboard.

`tests/test_bridge.py` is the Phase 1 gate: it asserts the extension is a genuine
`abi3` shared object and that `version()` is a built-in function coming from the
compiled module, not a Python shim. The rest of the suite stubs the extension, so
it runs even before the wheel is built — and stubs it with a real object that
returns the exact dict shape the bindings produce, so the tests still cover the
conversion and ranking code.

## Decisions worth knowing

1. **Python is pinned to 3.12** to match the `python:3.12-slim` API image, and the
   wheel is built with **`abi3-py312`**. One wheel therefore loads on CPython 3.12
   through 3.15, on the dev interpreter and in the container. Note the system
   interpreter here is 3.14; the pin exists so the dev and container environments
   cannot diverge.

2. **`pyo3` is an optional dependency behind a feature, and `extension-module` is
   off by default.** Without this, `cargo test -p search-core` tries to link
   `libpython` and fails on a machine whose system interpreter is newer than the
   one PyO3 builds against. The Rust logic is tested with no Python involved at
   all; maturin turns the bindings on for wheel builds.

3. **The release profile lives in `[tool.maturin] profile = "release"`**, not in a
   flag. `MATURIN_PEP517_ARGS="--release"` is rejected by maturin's pep517
   subcommand, and the silent consequence is a debug extension: correct, and
   several times slower than the code it replaces.

4. **After editing Rust, `uv sync` alone often rebuilds nothing.** uv keys its
   cached wheel on the project version, so a source-only change is invisible to it
   and you keep running the previous extension. `scripts/build.sh` uses
   `uv sync --reinstall-package search-core`; that is the reliable path.

5. **The query cache is stamped with an index fingerprint, not
   `Searcher::generation_id()`.** That counter is bumped by every `reload()`,
   changes or not, so using it as a cache key makes the cache miss on every query
   while looking perfectly correct. The fingerprint is a hash of the segment map,
   which changes only when a commit actually alters the index.

6. **Ranking boosts, then sorts, then caps.** The order is the whole policy: a
   freshness multiplier that is never followed by a sort changes nothing, because
   the engine already returned its page in its own order. The sort is stable, so
   once the multiplier underflows to zero for a page of old documents the engine's
   own order still decides.

7. **`crate-type = ["cdylib", "rlib"]`.** The `rlib` is required for `cargo test` to
   link the crate, and it lets the indexer reuse the very same `DocWriter` as the
   Python extension instead of reimplementing it.

8. **Two schema deviations**, both because later phases require them and because a
   schema change means a full reindex:
   * `url` is `STRING | STORED` (raw tokenizer). Upsert by URL needs an indexed
     unique key; stored-only would make add-or-update impossible.
   * `body` is `TEXT | STORED`. `SnippetGenerator::snippet_from_doc` reads the
     matched text back out of the *stored* document, so `TEXT` alone cannot
     produce snippets. This is also why `fetched_at` is `FAST | STORED`: reading it
     back from a fast field costs a columnar lookup, and 8 stored bytes against a
     kilobyte body is not a trade worth making.

9. **uvicorn runs 2-4 workers, and they must spawn rather than fork.** Every worker
   opens the same read-only index and mmaps it; the OS page cache shares those
   pages across processes, so extra workers add CPU contention on the same pages
   without buying memory headroom. The dashboard is the exception at one worker,
   for the per-process analytics reason above.

10. **Settings field names must match the environment variables they read.**
    pydantic-settings derives the variable from the field name, so
    `crawl_stats_path` would read `SEARCH_CRAWL_STATS_PATH` and silently ignore the
    `SEARCH_CRAWL_STATS` the crawler actually writes. Hence `crawl_stats`.

11. **Crawler errors carry their whole cause chain.** reqwest's own `Display` says
    only "error sending request for url (...)"; the cause that identifies the
    fault — refused connection, DNS failure, TLS verification — is one level down.
    Without it every transport failure in the log looks identical, which is how a
    test-harness mistake was briefly indistinguishable from a crawler bug.

12. **No `rust-toolchain.toml`.** It is a rustup-only mechanism and would be inert
    here, since this machine uses the system Rust toolchain.

13. **A crawl of a site is scoped, and the scope is enforced before the fetch.**
    `--stay-on-site` decides which links may be queued, so the budget is spent on
    the site the operator named instead of on the web it sits in. Hosts match
    exactly rather than by suffix — `docs.github.com` is a different host on
    different servers with its own robots.txt — which is also why redirects are
    counted (`redirected_offsite`) rather than silently followed: following them
    is usually right, but "I crawled github.com" and "I crawled github.com and its
    redirect targets" are different claims and the log should say which one it is.

14. **URLs are canonicalised before they enter the frontier — and again after a
    redirect.** Campaign parameters and tracking ids turn one document into five
    URLs, so they are stripped on the way in. The second pass is the one that is
    easy to miss: a redirect target can carry decoration the URL we asked for did
    not, and google.com's `gmail/about/for-work/` 301s to a workspace page with a
    full `utm_*` campaign attached. Archiving the decoration put the same document
    in the index twice under a URL nobody would type, which is exactly what the
    `.freebuff/run.md` crawl is checked for.

15. **The seed list comes from each site's own sitemap, and it is quota'd.**
    `scripts/make_seeds.py` reads `robots.txt` for `Sitemap:` entries, falls back to
    the conventional `/sitemap.xml`, and interleaves the results with a per-site
    ceiling. Link-following alone spends a crawl's budget on whatever the first page
    happens to link to; a sitemap is the site's own list of what it wants indexed.
    Interleaving is not cosmetic either: concatenating the lists produced 1000 URLs
    that were twenty hosts taking 49 each and `react.dev` taking one. The wikis are
    the exception that proves the rule — their sitemap endpoint 403s this bot, so
    Wikipedia, Wiktionary and Wikidata are read through the MediaWiki API instead,
    where 34 broad subjects are opened up one level into their subcategories, because
    `Sports` is a shelf and `Biathlon` is filed underneath it.

    The niche sources are chosen by probing rather than by taste: a candidate is
    added only if its `robots.txt` lets this bot read and it publishes a sitemap. A
    seed that is refused spends a request and returns nothing.

16. **Character references are decoded, including the common named ones.** A
    whitespace-collapsed title is what a result shows, and `&mdash;` in one is
    ordinary prose. The table is the ~200 references that actually appear in page
    text — dashes, quotes, accents, currency, arrows, Greek — not the full HTML5
    list of 2231, and anything missing is left literal rather than dropped: wrong but
    legible beats a silently corrupted URL. An undecoded numeric reference is a
    different failure mode and is handled separately (`&#8212;`, `&#x27;`).

17. **The per-site cap is a property of a page, not of the pool.** It used to be
    applied once to the whole candidate window, which made paging impossible in the
    case it exists for: `github` matches 16 791 documents, the top 400 by BM25 came
    from four hosts, and a two-per-site cap over all 400 left eight results — page
    two asked for offset ten and got an empty list, under a pager offering 1 172
    pages. Widening the window does not help (3 200 candidates took 603 ms and were
    still from nine hosts), because the cap was the binding constraint. Per page,
    pages are built by repeated passes over what is left of the pool, each capped
    independently, so no result is skipped and every page is diverse on its own.

18. **The response says what paging can actually reach.** `total` counts the index;
    `reachable` counts the ranked pool and `pages` counts the pages that pool produces
    at the requested limit — walked, not divided, because a page may hold fewer than
    `limit` results. The client pages by `pages` and shows `total` as "about N", which
    is the honest version of a number that is three orders of magnitude larger.

19. **A page short of its limit is still re-asked with a wider window.** The retry is
    triggered by that outcome rather than guessed at from the window's shape, and only
    when the engine reports matches beyond it — so an ordinary query still costs exactly
    one engine call and one cache entry. It is not what fixes deep paging; `reachable`
    reports that bound instead of pretending it does not exist.

20. **The UI audit measures per line box, and knows two exceptions.** An inline
    element that wraps has one rect per line and `getBoundingClientRect` returns
    their union, which overlaps the lines beside it without anything being on top of
    anything — that reported a wrapped `<code>` in a sentence as a collision. The two
    exceptions are the overlays a component declares (`data-overlay`: the search
    field's mirror, placeholder and clear button) and inline links in prose, which
    WCAG 2.5.8 itself exempts from the touch minimum.

21. **A site's own search box is not a document.** `home.cern/?s=`,
    `nasa.gov/?search=artemis`, `apple.com/us/search`: their text is whatever was
    searched for, they are unbounded (every query makes another one), and they were
    ranking second for topics the site does not otherwise discuss. Demoted while
    ranking by the same lever as the legal pages, and recognised in both spellings —
    a `/search` path segment and a search parameter in the query.

## Open questions

* **Sharding.** The index path already carries a `shard-000` component so that
  adding shards later is a configuration change. Whether queries should fan out
  across shards and merge, or whether the crawler should route documents to shards
  by domain, is unresolved — and it is worth deciding before the index is large
  enough for the answer to be expensive.

* **Boilerplate removal** is a structural filter (`nav`, `header`, `footer`,
  `aside`, `script`, …) rather than a readability model, because a model needs the
  whole document in memory. Worth revisiting if extraction quality proves
  insufficient.

* **Cross-worker analytics.** The dashboard's counters are per-process. A shared
  counter would make them global at the cost of a service dependency and network
  round trips on the query path, which is why it is not there — but if the API is
  ever scaled past a couple of workers, the honesty of the panels becomes the
  constraint rather than the counters.

## Roadmap

| Phase | Scope |
|---|---|
| 1 ✅ | Workspace, `Document`, schema, PyO3 bridge, API skeleton, tooling |
| 2 ✅ | Frontier, bloom dedup, robots, fetcher, `lol_html` extraction, WARC, spool |
| 3 ✅ | Query parser, BM25, pagination, snippets, module-load index handle, fingerprinted cache |
| 4 ✅ | `/search` end to end, `took_ms`, `/metrics` index stats, freshness + domain diversity |
| 4+ ✅ | Dashboard on :5000, query analytics, crawler progress, start-up chime |
| 5 | Validated multi-stage container images and a compose run |
| 5+ ✅ | Ubuntu server deployment: `deploy/setup-server.sh`, four systemd units, autostart |
