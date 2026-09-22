//! Runs the crawler.
//!
//! A long-lived standalone process, deliberately not driven through the API:
//! crawling is minutes-to-days of work and has no business sharing an event loop,
//! a process lifetime or a failure domain with request serving.
//!
//! ## Shape of the loop
//!
//! One owner task holds every piece of mutable state -- the frontier, the bloom
//! filter, the archive writer -- and a small pool of spawned tasks does the
//! network work. Finished work comes back over a channel. That arrangement keeps
//! the database and the WARC writer single-threaded without a mutex, and it makes
//! the memory bound easy to state: the only per-request state that exists at once
//! is one in-flight page per worker.
//!
//! ## Politeness
//!
//! Two independent limits, both enforced by the owner task:
//!
//! * the frontier refuses to hand out a URL whose host is inside its politeness
//!   window, so a host is never contacted faster than its `Crawl-delay` or our
//!   default allows;
//! * the dispatcher declines to hand out more than `--per-host-concurrency` URLs
//!   for a host that already has that many requests in flight. The skipped URL is
//!   not lost; it stays in the frontier.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

use common::Document;
use common::spool::CrawlJob;
use crawler::db::{CrawlDb, DbConfig};
use crawler::dedup::{DedupConfig, SeenUrls};
use crawler::fetcher::{FetchConfig, FetchedPage, Fetcher};
use crawler::frontier::{Frontier, host_of, politeness_delay_ms};
use crawler::parser::{ParsedPage, parse};
use crawler::policy::{self, CrawlScope};
use crawler::robots::{CACHE_TTL_MS, RobotsCache, RobotsRules, UNAVAILABLE_TTL_MS, robots_url};
use crawler::storage::{ArchiveWriter, StorageConfig};
use crawler::{CrawlError, sitemap};

/// Command-line configuration for a crawl.
#[derive(Debug, Parser)]
#[command(
    name = "crawler",
    version,
    about = "Async crawler: archives pages as WARC and spools them for indexing."
)]
struct Cli {
    /// Seed URLs to start from. Repeatable.
    #[arg(value_name = "URL")]
    seeds: Vec<String>,

    /// A file of seed URLs, one per line.
    ///
    /// Blank lines and `#` comments are skipped, so a seed list can be grouped
    /// and annotated. This exists because a command line is bounded and a real
    /// seed list is not: a thousand pages do not fit in an argument list, and
    /// re-typing them is not something anyone does twice. Seeds from the file are
    /// used in addition to any passed as arguments, not instead of them.
    #[arg(long, value_name = "PATH")]
    seed_file: Option<PathBuf>,

    /// Maximum link depth from a seed.
    #[arg(long, default_value_t = 3)]
    max_depth: u32,

    /// Global cap on requests in flight across all hosts.
    #[arg(long, default_value_t = 32)]
    workers: usize,

    /// Maximum simultaneous requests against any single host.
    #[arg(long, default_value_t = 2)]
    per_host_concurrency: usize,

    /// Minimum delay between two requests to the same host, in milliseconds.
    ///
    /// Always applied, and raised to a site's `Crawl-delay` when it asks for more.
    #[arg(long, default_value_t = 1_000)]
    politeness_delay_ms: u64,

    /// Stop after this many successful pages. Zero means run until interrupted.
    #[arg(long, default_value_t = 0)]
    max_pages: u64,

    /// Expected total URL count, used to size the seen-URL filter.
    #[arg(long, default_value_t = 5_000_000)]
    expected_urls: usize,

    /// Directory for RocksDB state: the frontier and the robots.txt cache.
    #[arg(long, env = "SEARCH_ROCKSDB_DIR", default_value = "data/rocksdb")]
    rocksdb_dir: PathBuf,

    /// Directory for compressed WARC output.
    #[arg(long, env = "SEARCH_WARC_DIR", default_value = "data/warc")]
    warc_dir: PathBuf,

    /// Directory for spool segments handed to the indexer.
    #[arg(long, env = "SEARCH_SPOOL_DIR", default_value = "data/spool")]
    spool_dir: PathBuf,

    /// Path of the persisted seen-URL filter.
    #[arg(long, env = "SEARCH_BLOOM_PATH", default_value = "data/urls.bloom")]
    bloom_path: PathBuf,

    /// Path of the progress file the dashboard reads.
    #[arg(
        long,
        env = "SEARCH_CRAWL_STATS",
        default_value = "data/crawl-stats.json"
    )]
    stats_path: PathBuf,

    /// RocksDB block cache per column family, in MiB.
    #[arg(long, default_value_t = 32)]
    block_cache_mb: u64,

    /// RocksDB write buffer per column family, in MiB.
    #[arg(long, default_value_t = 16)]
    write_buffer_mb: u64,

    /// Largest response body accepted, in MiB.
    #[arg(long, default_value_t = 8)]
    max_body_mib: usize,

    /// Roll to a new spool segment after this many MiB.
    #[arg(long, default_value_t = 8)]
    segment_mib: u64,

    /// `User-Agent` sent with every request.
    #[arg(
        long,
        env = "SEARCH_USER_AGENT",
        default_value = "MJLsearchBot/0.1 (+https://example.invalid/bot)"
    )]
    user_agent: String,

    /// Follow only links whose host is one of the seed hosts.
    ///
    /// This is what makes a crawl of a site a crawl of *that* site. Without it
    /// the crawler wanders off the first page it fetches and spends its budget
    /// on the rest of the web, which is how a crawl aimed at one site ends up
    /// indexing almost none of it.
    #[arg(long, default_value_t = false)]
    stay_on_site: bool,

    /// An extra host to follow. Repeatable, and implies `--stay-on-site`.
    ///
    /// Hosts are matched exactly, so `docs.github.com` has to be named
    /// alongside `github.com`: a subdomain is a different site on different
    /// servers, with its own robots.txt and its own rate limits.
    #[arg(long, value_name = "HOST")]
    allow_host: Vec<String>,

    /// With `--stay-on-site`, also drop pages whose redirect target is off-site.
    ///
    /// Off by default, because following a redirect is what a crawler is for and
    /// the target is usually the canonical home of the content -- Google's own
    /// `www.google.com/intl/en/about/products/` lands on `about.google`, which is
    /// genuinely part of the same site. Turn it on for a crawl that must provably
    /// stay inside its hosts, and watch the counter either way.
    #[arg(long, default_value_t = false)]
    drop_offsite_redirects: bool,
}

/// Counters describing the run.
#[derive(Debug, Clone, Default)]
struct Stats {
    queued: u64,
    fetched: u64,
    indexed: u64,
    failed: u64,
    disallowed: u64,
    /// Sitemaps read, which discovered URLs rather than documents.
    sitemaps: u64,
    /// Links dropped because they left the crawl's scope.
    skipped_offsite: u64,
    /// Links dropped because they are not HTML: images, archives, media.
    skipped_assets: u64,
    /// Pages whose final URL, after redirects, was outside the crawl's scope.
    redirected_offsite: u64,
}

/// Settings the completions path needs.
struct Settings {
    max_depth: u32,
    politeness_delay_ms: u64,
    scope: CrawlScope,
    drop_offsite_redirects: bool,
}

/// What a worker task reports back.
struct Outcome {
    host: String,
    job: CrawlJob,
    /// Rules to be cached by the owner, when this visit learned them.
    rules: Option<RobotsRules>,
    /// How long those rules stay fresh. Always set when `rules` is.
    rules_ttl_ms: Option<i64>,
    /// The delay that applies to this host, from robots.txt or our default.
    crawl_delay_s: Option<u64>,
    /// Sitemaps the host declared, to be enqueued by the owner.
    sitemaps: Vec<String>,
    result: Result<Visited, CrawlError>,
}

/// What a fetched URL turned out to be.
#[derive(Debug)]
enum Visited {
    /// An HTML document, ready to archive.
    Page {
        page: FetchedPage,
        parsed: ParsedPage,
    },
    /// A sitemap: a list of URLs. There is no document here to index, and
    /// treating it as one would put a raw XML file into the search results.
    Sitemap { url: String, links: Vec<String> },
}

/// Whether a response is a document or a sitemap.
///
/// The declared content type is trusted first, because a page may perfectly well
/// live at a URL ending in `.xml` and a sitemap may be served without any
/// helpful suffix at all. The URL is only the fallback, for the servers that
/// send no usable type.
fn classify(final_url: &str, content_type: Option<&str>) -> Kind {
    match content_type {
        Some(value) if value.to_ascii_lowercase().contains("html") => Kind::Page,
        Some(value) if policy::is_xml_content_type(Some(value)) => Kind::Sitemap,
        _ if policy::is_sitemap_url(final_url) => Kind::Sitemap,
        _ => Kind::Page,
    }
}

/// The two things a URL can turn out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Page,
    Sitemap,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    let cli = Cli::parse();

    match run(cli).await {
        Ok(stats) => {
            tracing::info!(
                queued = stats.queued,
                fetched = stats.fetched,
                indexed = stats.indexed,
                failed = stats.failed,
                disallowed = stats.disallowed,
                sitemaps = stats.sitemaps,
                skipped_offsite = stats.skipped_offsite,
                skipped_assets = stats.skipped_assets,
                redirected_offsite = stats.redirected_offsite,
                "crawl finished"
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!(error = %error, "crawl failed");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<Stats, CrawlError> {
    let db = CrawlDb::open(
        &cli.rocksdb_dir,
        &DbConfig {
            block_cache_mb: cli.block_cache_mb,
            write_buffer_mb: cli.write_buffer_mb,
            ..DbConfig::default()
        },
    )?;

    let dedup = DedupConfig {
        expected_urls: cli.expected_urls,
        false_positive_rate: 0.01,
    };

    let fetch_config = FetchConfig {
        max_concurrent_per_host: cli.per_host_concurrency,
        user_agent: cli.user_agent.clone(),
        max_body_bytes: cli.max_body_mib * 1024 * 1024,
        ..FetchConfig::default()
    };

    let seeds = collect_seeds(&cli)?;

    let scope = CrawlScope::new(cli.stay_on_site, &seeds, &cli.allow_host);
    let settings = Settings {
        max_depth: cli.max_depth,
        politeness_delay_ms: cli.politeness_delay_ms,
        scope,
        drop_offsite_redirects: cli.drop_offsite_redirects,
    };

    let mut frontier = Frontier::open(&db)?;
    let mut seen = SeenUrls::open(&cli.bloom_path, dedup)?;
    let robots = RobotsCache::new(&db);
    let mut archive = ArchiveWriter::open(StorageConfig {
        warc_dir: cli.warc_dir.clone(),
        spool_dir: cli.spool_dir.clone(),
        segment_bytes: cli.segment_mib * 1024 * 1024,
        ..StorageConfig::default()
    })?;
    let fetcher = Arc::new(Fetcher::new(fetch_config)?);

    tracing::info!(
        seeds = seeds.len(),
        max_depth = cli.max_depth,
        workers = cli.workers,
        per_host_concurrency = cli.per_host_concurrency,
        politeness_delay_ms = cli.politeness_delay_ms,
        restricted_to = ?settings.scope.hosts(),
        expected_urls = cli.expected_urls,
        bloom_mib = dedup.bytes() / (1024 * 1024),
        bloom_hashes = dedup.hash_functions(),
        rocksdb_dir = %cli.rocksdb_dir.display(),
        warc_dir = %cli.warc_dir.display(),
        spool_dir = %cli.spool_dir.display(),
        "crawler starting"
    );

    let mut stats = Stats::default();
    for seed in &seeds {
        match normalize_seed(seed) {
            Ok(url) => {
                if seen.check_and_insert(&url) {
                    frontier.push(&CrawlJob { url, depth: 0 })?;
                    stats.queued += 1;
                }
            }
            Err(error) => {
                // A bad seed is the operator's typo, not a reason to abandon the
                // seeds that are fine.
                tracing::warn!(seed = %seed, error = %error, "seed rejected");
            }
        }
    }

    // Nothing queued from the seeds is normal on a re-run: the frontier keeps its
    // pending work between runs, so an already-crawled seed list is the usual way to
    // continue one. What is not normal is nothing queued *and* nothing pending, which
    // is a crawl that will fetch nothing and exit zero -- indistinguishable, in the
    // summary, from a successful crawl of a site that had nothing new. It is the state
    // a partially wiped corpus leaves behind: the frontier lives in RocksDB but the
    // seen-set lives in the bloom file, so deleting one and not the other makes the
    // crawler believe the whole list has already been fetched. That took a log
    // inspection to notice, which is exactly the kind of thing that should say itself.
    let pending = frontier.pending_count()?;
    if stats.queued == 0 {
        if pending == 0 {
            tracing::warn!(
                seeds = seeds.len(),
                bloom_path = %cli.bloom_path.display(),
                "every seed is already known and the frontier is empty, so this run has \
                 nothing to fetch. To re-crawl URLs that have been seen before, remove \
                 the bloom filter along with the frontier: it is the seen-set, not a cache."
            );
        } else {
            tracing::info!(
                seeds = seeds.len(),
                pending,
                "no new seeds from the list; continuing with the frontier"
            );
        }
    }

    let (sender, mut receiver) = mpsc::channel::<Outcome>(cli.workers * 2);
    let mut in_flight: HashMap<String, usize> = HashMap::new();
    let mut in_flight_total = 0usize;
    let mut shutting_down = false;

    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    let mut stats_tick = tokio::time::interval(Duration::from_secs(2));
    let mut last_stats_write = std::time::Instant::now();

    loop {
        // 1. Absorb everything already finished.
        while let Ok(outcome) = receiver.try_recv() {
            bookkeep(&outcome, &mut in_flight, &mut in_flight_total);
            absorb(
                outcome,
                &mut frontier,
                &mut seen,
                &robots,
                &mut archive,
                &mut stats,
                &settings,
            )?;
        }

        // 2. Decide whether to keep going.
        if cli.max_pages > 0 && stats.indexed >= cli.max_pages && !shutting_down {
            tracing::info!(max_pages = cli.max_pages, "page budget reached");
            shutting_down = true;
        }
        if shutting_down && in_flight_total == 0 {
            break;
        }

        // 3. Dispatch while there is room.
        while !shutting_down && in_flight_total < cli.workers {
            let now = now_ms();
            // Snapshotted into owned strings so the immutable borrow of in_flight
            // does not outlive this statement.
            let saturated: HashSet<String> = in_flight
                .iter()
                .filter(|(_, count)| **count >= cli.per_host_concurrency)
                .map(|(host, _)| host.clone())
                .collect();

            let Some(job) = frontier.pop_ready(now, 128, &|host| saturated.contains(host))? else {
                break;
            };

            let Some(host) = host_of(&job.url) else {
                continue;
            };
            let rules = robots.get(&host, now)?;
            let delay = rules.as_ref().and_then(|rules| rules.crawl_delay_s);

            *in_flight.entry(host.clone()).or_insert(0) += 1;
            in_flight_total += 1;

            let sender = sender.clone();
            let fetcher = Arc::clone(&fetcher);

            tokio::spawn(async move {
                let outcome = visit(fetcher, job, host, rules, delay).await;
                // A send failure means the owner has gone; the task simply ends.
                let _ = sender.send(outcome).await;
            });
        }

        // 4. Publish progress, then wait for something to happen.
        if last_stats_write.elapsed() >= Duration::from_secs(2) {
            last_stats_write = std::time::Instant::now();
            let pending = frontier.pending_count()?;
            let hosts = frontier.host_count()?;
            if let Err(error) = write_stats_file(&cli.stats_path, &stats, pending, hosts) {
                tracing::debug!(error = %error, "could not write progress file");
            }
        }

        if in_flight_total == 0 && !shutting_down {
            // Nothing outstanding and nothing dispatched: either the frontier is
            // drained, or every remaining host is inside its politeness window.
            if frontier.pending_count()? == 0 {
                tracing::info!("frontier drained");
                break;
            }
            tokio::select! {
                _ = &mut interrupt, if !shutting_down => {
                    tracing::info!("interrupt received; finishing outstanding requests");
                    shutting_down = true;
                }
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
            continue;
        }

        tokio::select! {
            _ = &mut interrupt, if !shutting_down => {
                tracing::info!("interrupt received; finishing outstanding requests");
                shutting_down = true;
            }
            received = receiver.recv() => {
                if let Some(outcome) = received {
                    bookkeep(&outcome, &mut in_flight, &mut in_flight_total);
                    absorb(
                        outcome,
                        &mut frontier,
                        &mut seen,
                        &robots,
                        &mut archive,
                        &mut stats,
                        &settings,
                    )?;
                }
            }
            _ = stats_tick.tick() => {}
        }
    }

    // Shutdown: seal the spool so the indexer can pick everything up, flush the
    // WARC file, and persist the filter so a restart does not re-crawl.
    archive.seal()?;
    seen.save()?;
    let pending = frontier.pending_count()?;
    write_stats_file(&cli.stats_path, &stats, pending, frontier.host_count()?)?;

    tracing::info!(
        documents = archive.stats().documents,
        sealed_segments = archive.stats().sealed_segments,
        warc_records = archive.stats().warc_records,
        warc_mib = archive.stats().warc_bytes / (1024 * 1024),
        frontier_pending = pending,
        "crawl stopped"
    );

    Ok(stats)
}

/// One visit: robots, fetch, parse. Never returns an error; failure is data.
async fn visit(
    fetcher: Arc<Fetcher>,
    job: CrawlJob,
    host: String,
    cached_rules: Option<RobotsRules>,
    crawl_delay_s: Option<u64>,
) -> Outcome {
    let mut rules = cached_rules;
    let mut newly_fetched = None;
    let mut rules_ttl_ms = None;
    let mut delay = crawl_delay_s;

    if rules.is_none() {
        // Every branch below caches its answer. The version that did not was slow
        // in a way that was hard to see: a host with no robots.txt re-attempted the
        // fetch for every single URL, so the crawl's throughput was one robots.txt
        // round trip per page rather than one per host.
        match robots_url(&job.url) {
            None => {}
            Some(url) => match fetch_robots(&fetcher, &url).await {
                Ok(Some(fetched)) => {
                    delay = fetched.crawl_delay_s;
                    rules_ttl_ms = Some(CACHE_TTL_MS);
                    newly_fetched = Some(fetched.clone());
                    rules = Some(fetched);
                }
                Ok(None) => {
                    // A 404 means the file does not exist, so nothing is disallowed.
                    rules = Some(RobotsRules::allow_all());
                    rules_ttl_ms = Some(CACHE_TTL_MS);
                    newly_fetched = Some(RobotsRules::allow_all());
                }
                Err(error) => {
                    // An unreachable robots.txt is treated as a full disallow, which
                    // is the conservative RFC 9309 reading and the one a real host
                    // wants: a 5xx is usually an origin in trouble or a rate limit,
                    // and the moment a site is struggling is precisely the wrong
                    // moment to decide that "no answer" means "no restrictions".
                    // Cached briefly, so a transient failure is retried without
                    // making the host pay for it on every URL in between.
                    tracing::debug!(host = %host, error = %error, "robots.txt unavailable");
                    rules = Some(RobotsRules::disallow_all());
                    rules_ttl_ms = Some(UNAVAILABLE_TTL_MS);
                    newly_fetched = Some(RobotsRules::disallow_all());
                }
            },
        }
    }

    // Only a freshly fetched file can teach us anything we have not already
    // acted on, and re-queuing the same sitemaps on every page would flood the
    // frontier with duplicates that the seen-URL filter then has to absorb.
    let sitemaps = match &newly_fetched {
        Some(rules) => rules.sitemaps.clone(),
        None => Vec::new(),
    };

    if let Some(rules) = &rules {
        let path = request_path(&job.url);
        if !rules.allows(&path) {
            tracing::debug!(url = %job.url, "skipped by robots.txt");
            return Outcome {
                host,
                job,
                rules: newly_fetched,
                rules_ttl_ms,
                crawl_delay_s: delay,
                sitemaps,
                result: Err(CrawlError::DisallowedByRobots),
            };
        }
    }

    let result = match fetcher.fetch(&job.url).await {
        Ok(page) => {
            let page = canonical_final_url(page);
            let base = page.final_url.clone();
            match classify(&base, page.content_type.as_deref()) {
                Kind::Sitemap => Ok(Visited::Sitemap {
                    url: page.final_url,
                    links: sitemap::parse(&page.body),
                }),
                Kind::Page => match parse(&page.body, &base) {
                    Ok(parsed) => Ok(Visited::Page { page, parsed }),
                    Err(error) => Err(error),
                },
            }
        }
        Err(error) => Err(error),
    };

    Outcome {
        host,
        job,
        rules: newly_fetched,
        rules_ttl_ms,
        crawl_delay_s: delay,
        sitemaps,
        result,
    }
}

/// Replaces the URL a redirect landed on with its canonical form.
///
/// A redirect is the one way a document's URL can differ from the URL the crawl
/// queued, and servers use that to decorate it: Google's
/// `www.google.com/intl/am/gmail/about/for-work/` lands on a workspace page
/// carrying a full `utm_*` campaign, and its `business-profile` page lands on
/// itself plus `?ucbcb=1`. Archiving the decoration would put one document in the
/// index several times under URLs nobody would ever type, and it would defeat
/// the seen-URL filter, since the decorated URL is new to it.
///
/// Canonicalising here also makes two redirect chains that converge on the same
/// page collapse into one document, which is the point of a canonical URL.
fn canonical_final_url(page: FetchedPage) -> FetchedPage {
    match policy::canonical_url(&page.final_url) {
        Some(canonical) => FetchedPage {
            final_url: canonical,
            ..page
        },
        None => page,
    }
}

/// Fetches and parses a robots.txt.
///
/// `Ok(None)` means the host has no rules -- a 404 is a normal, expected answer.
async fn fetch_robots(fetcher: &Fetcher, url: &str) -> Result<Option<RobotsRules>, CrawlError> {
    match fetcher.fetch_text(url).await {
        Ok(page) => Ok(Some(crawler::robots::parse(
            &page.body,
            &fetcher.config().user_agent,
        ))),
        Err(CrawlError::HttpStatus(404)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Applies one finished visit: caches rules, defers the host, archives the page
/// and schedules any new links.
#[allow(clippy::too_many_arguments)]
fn absorb(
    outcome: Outcome,
    frontier: &mut Frontier<'_>,
    seen: &mut SeenUrls,
    robots: &RobotsCache<'_>,
    archive: &mut ArchiveWriter,
    stats: &mut Stats,
    settings: &Settings,
) -> Result<(), CrawlError> {
    let now = now_ms();
    let Outcome {
        host,
        job,
        rules,
        rules_ttl_ms,
        crawl_delay_s,
        sitemaps,
        result,
    } = outcome;

    if let Some(rules) = &rules {
        let ttl = rules_ttl_ms.unwrap_or(CACHE_TTL_MS);
        robots.put_for(&host, rules, now, ttl)?;
    }

    // Deferred whether or not the fetch succeeded: a host that just failed should
    // not be hammered either.
    let delay = politeness_delay_ms(settings.politeness_delay_ms, crawl_delay_s);
    frontier.defer_host(&host, now.saturating_add(delay as i64), now)?;

    // Sitemaps are queued whether or not this particular page worked, and at
    // depth zero: a sitemap is a statement of what the site contains rather than
    // a link in its graph, so it should not be spent against the depth budget.
    if !sitemaps.is_empty() {
        let discovered = schedule(&sitemaps, 0, frontier, seen, stats, settings)?;
        tracing::debug!(
            host = %host,
            sitemaps = sitemaps.len(),
            discovered,
            "sitemaps declared by robots.txt"
        );
    }

    let visited = match result {
        Ok(visited) => visited,
        Err(CrawlError::DisallowedByRobots) => {
            // Not a failure. The site was asked politely and answered.
            stats.disallowed += 1;
            tracing::debug!(url = %job.url, "disallowed by robots.txt");
            return Ok(());
        }
        Err(error) => {
            stats.failed += 1;
            tracing::debug!(url = %job.url, error = %error, "page failed");
            return Ok(());
        }
    };

    // A redirect can land anywhere, and where it landed decides whether the
    // document is one we meant to collect. Counted rather than silently folded
    // in, because "I crawled github.com" and "I crawled github.com and whatever
    // it redirected me to" are different claims.
    let offsite_target = final_url_of(&visited)
        .filter(|url| settings.scope.is_restricted() && !settings.scope.allows(url))
        .map(str::to_string);
    if let Some(final_url) = &offsite_target {
        stats.redirected_offsite += 1;
        tracing::debug!(
            from = %job.url,
            to = %final_url,
            dropped = settings.drop_offsite_redirects,
            "redirect left the crawl's scope"
        );
        if settings.drop_offsite_redirects {
            return Ok(());
        }
    }

    match visited {
        Visited::Sitemap { url, links } => {
            stats.sitemaps += 1;
            // One hop from the seed, not zero: a sitemap is a discovery channel
            // rather than a link in the graph, but giving its URLs depth zero
            // would let a crawl of one seed pull in an unbounded number of pages
            // and make `--max-depth 0` mean nothing.
            let discovered = schedule(&links, SITEMAP_DEPTH, frontier, seen, stats, settings)?;
            tracing::info!(
                url = %url,
                listed = links.len(),
                discovered,
                "sitemap read"
            );
        }
        Visited::Page { page, parsed } => {
            stats.fetched += 1;

            // Worth a line whenever it happens, and not only when the target is
            // off-site: a redirect is the one way a page's identity can differ
            // from the URL the crawl asked for, so it is the first thing to look
            // at when the index contains a URL nobody queued.
            if page.final_url != job.url {
                tracing::debug!(
                    requested = %job.url,
                    landed = %page.final_url,
                    "redirect followed"
                );
            }

            let document = Document::new(
                page.final_url.clone(),
                parsed.title.clone(),
                parsed.text.clone(),
                now / 1_000,
                parsed.links.clone(),
            );
            archive.write_page(&page, &document)?;
            stats.indexed += 1;

            // Reported whether or not the links are followed, because "the crawl
            // stopped" and "the pages had no new links" are indistinguishable from
            // the outside, and only one of them is a bug.
            let discovered = if job.depth < settings.max_depth {
                schedule(
                    &parsed.links,
                    job.depth + 1,
                    frontier,
                    seen,
                    stats,
                    settings,
                )?
            } else {
                0
            };

            tracing::debug!(
                url = %page.final_url,
                depth = job.depth,
                links = parsed.links.len(),
                discovered,
                "page parsed"
            );
        }
    }

    Ok(())
}

/// The depth sitemap-listed URLs enter the frontier at.
const SITEMAP_DEPTH: u32 = 1;

/// The URL a visit actually ended on.
fn final_url_of(visited: &Visited) -> Option<&str> {
    match visited {
        Visited::Page { page, .. } => Some(&page.final_url),
        Visited::Sitemap { url, .. } => Some(url),
    }
}

/// Canonicalises, filters and enqueues a batch of links.
///
/// Every URL entering the frontier passes through here, so this is the one place
/// that decides what the crawl will and will not spend a request on. The order
/// matters: canonicalise before the dedup check, or the same document under two
/// spellings costs two fetches, and check the scope before anything else, or an
/// off-site link is counted as newly discovered work it is not.
fn schedule(
    links: &[String],
    depth: u32,
    frontier: &mut Frontier<'_>,
    seen: &mut SeenUrls,
    stats: &mut Stats,
    settings: &Settings,
) -> Result<usize, CrawlError> {
    let mut discovered = 0usize;

    for raw in links {
        let Some(url) = policy::canonical_url(raw) else {
            continue;
        };
        if !settings.scope.allows(&url) {
            stats.skipped_offsite += 1;
            continue;
        }
        if !policy::is_probably_page(&url) {
            stats.skipped_assets += 1;
            continue;
        }
        if seen.check_and_insert(&url) {
            frontier.push(&CrawlJob { url, depth })?;
            stats.queued += 1;
            discovered += 1;
        }
    }

    Ok(discovered)
}

/// Updates the in-flight accounting for a completed visit.
fn bookkeep(outcome: &Outcome, in_flight: &mut HashMap<String, usize>, total: &mut usize) {
    *total = total.saturating_sub(1);
    if let Some(count) = in_flight.get_mut(&outcome.host) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            in_flight.remove(&outcome.host);
        }
    }
}

/// Turns a seed into a canonical, fetchable URL.
///
/// Goes through the same canonicalisation as a discovered link, so a seed and a
/// link to the same page are one URL rather than two.
pub fn normalize_seed(seed: &str) -> Result<String, CrawlError> {
    let parsed = url::Url::parse(seed).map_err(|_| CrawlError::InvalidUrl {
        url: seed.to_string(),
        reason: "not an absolute URL",
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(CrawlError::InvalidUrl {
            url: seed.to_string(),
            reason: "only http and https can be crawled",
        });
    }
    policy::canonical_url(seed).ok_or(CrawlError::InvalidUrl {
        url: seed.to_string(),
        reason: "not a fetchable absolute URL",
    })
}

/// The path and query of a URL, which is what robots.txt rules match against.
pub fn request_path(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => match parsed.query() {
            Some(query) => format!("{}?{query}", parsed.path()),
            None => parsed.path().to_string(),
        },
        Err(_) => "/".to_string(),
    }
}

/// Publishes progress for the dashboard.
///
/// The API cannot read the crawler's RocksDB -- the crawler holds the directory
/// lock -- so progress travels as a small JSON file written through a temporary
/// path, which keeps a reader from ever seeing a half-written document.
fn write_stats_file(
    path: &std::path::Path,
    stats: &Stats,
    frontier_pending: u64,
    hosts: u64,
) -> Result<(), CrawlError> {
    let json = format!(
        "{{\"queued\":{},\"fetched\":{},\"indexed\":{},\"failed\":{},\"disallowed\":{},\
         \"sitemaps\":{},\"skipped_offsite\":{},\"skipped_assets\":{},\"redirected_offsite\":{},\
         \"frontier_pending\":{},\"hosts\":{},\"updated_at\":{}}}",
        stats.queued,
        stats.fetched,
        stats.indexed,
        stats.failed,
        stats.disallowed,
        stats.sitemaps,
        stats.skipped_offsite,
        stats.skipped_assets,
        stats.redirected_offsite,
        frontier_pending,
        hosts,
        now_ms() / 1_000,
    );

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Every seed to start from: the ones on the command line, then the ones in
/// `--seed-file`, in that order and without deduplication.
///
/// Duplicates cost nothing here: the seen-URL filter drops the second copy at the
/// moment it is queued, so a file that repeats a URL is harmless rather than a
/// reason to reject the list. What is checked is that the list is not empty -- a
/// crawl with no seeds would start, report success and index nothing, which is
/// the one failure mode that looks like a working system.
fn collect_seeds(cli: &Cli) -> Result<Vec<String>, CrawlError> {
    let mut seeds = cli.seeds.clone();

    if let Some(path) = &cli.seed_file {
        let contents = std::fs::read_to_string(path)?;
        let before = seeds.len();

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            seeds.push(line.to_string());
        }

        tracing::info!(
            path = %path.display(),
            urls = seeds.len() - before,
            "seed file read"
        );
    }

    if seeds.is_empty() {
        return Err(CrawlError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "no seeds: pass URLs as arguments or a list with --seed-file",
        )));
    }

    Ok(seeds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_are_normalised_and_fragments_dropped() {
        assert_eq!(
            normalize_seed("HTTPS://Example.com/a#section").expect("normalise"),
            "https://example.com/a"
        );
        assert_eq!(
            normalize_seed("http://example.com").expect("normalise"),
            "http://example.com/"
        );
    }

    #[test]
    fn unfetchable_seeds_are_rejected() {
        assert!(normalize_seed("not a url").is_err());
        assert!(normalize_seed("mailto:someone@example.com").is_err());
        assert!(normalize_seed("ftp://example.com/file").is_err());
        assert!(normalize_seed("file:///etc/passwd").is_err());
    }

    #[test]
    fn a_seed_file_adds_to_the_seeds_on_the_command_line() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("seeds.txt");
        std::fs::write(
            &path,
            "# group one\n\nhttps://example.com/a\n  https://example.com/b  \n",
        )
        .expect("write");

        let cli = Cli::parse_from([
            "crawler",
            "https://first.example/",
            "--seed-file",
            path.to_str().expect("utf8"),
        ]);
        let seeds = collect_seeds(&cli).expect("seeds");

        // Comments and blank lines are structure, not seeds; the argument seeds
        // come first and the file adds to them rather than replacing them.
        assert_eq!(
            seeds,
            vec![
                "https://first.example/",
                "https://example.com/a",
                "https://example.com/b"
            ]
        );
    }

    #[test]
    fn a_crawl_without_seeds_is_an_error_not_an_empty_run() {
        // Starting with nothing to fetch would report a clean exit and index
        // nothing, which is indistinguishable from a working crawl.
        let cli = Cli::parse_from(["crawler"]);
        assert!(collect_seeds(&cli).is_err());
    }

    #[test]
    fn a_missing_seed_file_is_reported_rather_than_ignored() {
        let cli = Cli::parse_from(["crawler", "--seed-file", "/nonexistent/seeds.txt"]);
        assert!(collect_seeds(&cli).is_err());
    }

    #[test]
    fn request_path_includes_the_query_and_defaults_to_root() {
        assert_eq!(
            request_path("https://example.com/a/b?x=1&y=2"),
            "/a/b?x=1&y=2"
        );
        assert_eq!(request_path("https://example.com/a/b"), "/a/b");
        assert_eq!(request_path("https://example.com"), "/");
        assert_eq!(request_path("nonsense"), "/");
    }

    #[test]
    fn the_progress_file_is_valid_json() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("stats.json");

        let stats = Stats {
            queued: 5,
            fetched: 3,
            indexed: 2,
            failed: 1,
            disallowed: 0,
            sitemaps: 1,
            skipped_offsite: 11,
            skipped_assets: 4,
            redirected_offsite: 2,
        };
        write_stats_file(&path, &stats, 7, 2).expect("write");

        let contents = std::fs::read_to_string(&path).expect("read");
        let parsed: serde_json::Value = serde_json::from_str(&contents).expect("valid json");

        assert_eq!(parsed["indexed"], 2);
        assert_eq!(parsed["frontier_pending"], 7);
        assert_eq!(parsed["hosts"], 2);
        // The counters that explain why a crawl is smaller than its link graph.
        assert_eq!(parsed["sitemaps"], 1);
        assert_eq!(parsed["skipped_offsite"], 11);
        assert_eq!(parsed["skipped_assets"], 4);
        assert_eq!(parsed["redirected_offsite"], 2);
        // No temporary file should be left behind for a reader to trip over.
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn in_flight_accounting_releases_a_host_when_its_last_request_finishes() {
        let mut in_flight = HashMap::new();
        let mut total = 0usize;

        in_flight.insert("a.example".to_string(), 2);
        total += 2;

        let outcome = Outcome {
            host: "a.example".to_string(),
            job: CrawlJob {
                url: "https://a.example/".to_string(),
                depth: 0,
            },
            rules: None,
            rules_ttl_ms: None,
            crawl_delay_s: None,
            sitemaps: Vec::new(),
            result: Err(CrawlError::HttpStatus(404)),
        };

        bookkeep(&outcome, &mut in_flight, &mut total);
        assert_eq!(in_flight.get("a.example"), Some(&1));
        assert_eq!(total, 1);

        // The entry must disappear at zero, or the map would grow with every host
        // ever visited.
        bookkeep(&outcome, &mut in_flight, &mut total);
        assert!(!in_flight.contains_key("a.example"));
        assert_eq!(total, 0);
    }

    #[test]
    fn a_disallowed_page_is_reported_as_a_failure_not_indexed() {
        let rules = crawler::robots::parse("User-agent: *\nDisallow: /private\n", "MJLsearchBot");
        assert!(!rules.allows("/private/secret"));
        assert!(rules.allows("/public"));
    }

    #[test]
    fn a_redirect_target_is_stored_canonically() {
        // Measured on a real chain: google.com's `gmail/about/for-work/` 301s to a
        // workspace page with a full utm campaign attached, and archiving the
        // decoration put the same document in the index under a URL nobody typed.
        let page = FetchedPage {
            final_url: "https://workspace.google.com/intl/am/products/gmail/\
                 ?utm_source=gmailforwork&utm_medium=et&utm_campaign=global"
                .to_string(),
            status: 200,
            content_type: Some("text/html".to_string()),
            body: String::new(),
        };

        assert_eq!(
            canonical_final_url(page).final_url,
            "https://workspace.google.com/intl/am/products/gmail/"
        );
    }

    #[test]
    fn a_redirect_target_that_is_already_canonical_is_left_alone() {
        let page = FetchedPage {
            final_url: "https://example.com/docs?page=2".to_string(),
            status: 200,
            content_type: None,
            body: String::new(),
        };
        assert_eq!(
            canonical_final_url(page).final_url,
            "https://example.com/docs?page=2"
        );
    }

    #[test]
    fn a_seed_loses_its_campaign_parameters() {
        // A seed and a discovered link to the same page must be one URL, or the
        // bloom filter sees two documents where there is one.
        assert_eq!(
            normalize_seed("https://example.com/?utm_source=x").expect("normalise"),
            "https://example.com/"
        );
    }

    #[test]
    fn a_response_is_classified_by_its_content_type_before_its_url() {
        // A page may perfectly well live at a URL ending in `.xml`.
        assert_eq!(
            classify(
                "https://example.com/feed.xml",
                Some("text/html; charset=utf-8")
            ),
            Kind::Page
        );
        assert_eq!(
            classify("https://example.com/sitemap", Some("application/xml")),
            Kind::Sitemap
        );
        assert_eq!(
            classify("https://example.com/docs", Some("application/xhtml+xml")),
            Kind::Page
        );
        // With no usable type the URL is all there is to go on.
        assert_eq!(
            classify("https://example.com/sitemap.xml", None),
            Kind::Sitemap
        );
        assert_eq!(classify("https://example.com/docs", None), Kind::Page);
    }

    #[test]
    fn scheduling_enqueues_only_what_the_scope_allows() {
        use crawler::db::DbConfig;

        let directory = tempfile::tempdir().expect("tempdir");
        let db = CrawlDb::open(directory.path(), &DbConfig::default()).expect("open");
        let mut frontier = Frontier::open(&db).expect("open");
        let bloom = directory.path().join("seen.bloom");
        let mut seen = SeenUrls::open(
            &bloom,
            DedupConfig {
                expected_urls: 1_000,
                false_positive_rate: 0.01,
            },
        )
        .expect("open");

        let settings = Settings {
            max_depth: 3,
            politeness_delay_ms: 0,
            scope: CrawlScope::new(true, &["https://github.com/".to_string()], &[]),
            drop_offsite_redirects: false,
        };
        let mut stats = Stats::default();

        let links = vec![
            "https://github.com/features?utm_source=nav".to_string(),
            "https://github.com/features".to_string(),
            "https://news.example/github".to_string(),
            "https://github.com/logo.png".to_string(),
        ];

        let discovered =
            schedule(&links, 1, &mut frontier, &mut seen, &mut stats, &settings).expect("schedule");

        // One page: the campaign-tagged duplicate and the plain link are the same
        // document, the off-site link is out of scope, and the image is not a page.
        assert_eq!(discovered, 1);
        assert_eq!(stats.queued, 1);
        assert_eq!(stats.skipped_offsite, 1);
        assert_eq!(stats.skipped_assets, 1);
        assert_eq!(frontier.pending_count().expect("count"), 1);
    }
}
