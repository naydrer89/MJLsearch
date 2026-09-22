//! robots.txt fetching, parsing and caching.
//!
//! Rules are cached per host with a 24 hour TTL, in RocksDB rather than in a map.
//! A long crawl touches millions of hosts, and a per-host in-memory map would
//! grow without bound for exactly the same reason the frontier does not live in a
//! `VecDeque`.
//!
//! The matcher implements the parts of RFC 9309 that get crawlers blocked when
//! they are missing: longest-match between `Allow` and `Disallow`, `*` wildcards,
//! `$` end anchors, and per-user-agent group selection. A parser that ignores
//! wildcards over-permits, and over-permitting is the direction that gets a
//! crawler shut out.

use serde::{Deserialize, Serialize};

use crate::CrawlError;
use crate::db::{CF_ROBOTS, CrawlDb};

/// How long a fetched robots.txt is trusted before it is refetched.
pub const CACHE_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

/// How long a *failed* robots.txt fetch is remembered.
///
/// Short, because the failure may be transient, but not zero: without it every
/// single URL of the host pays for the fetch attempt again, which is both slower
/// than the crawl needs to be and more requests than the host was promised.
pub const UNAVAILABLE_TTL_MS: i64 = 5 * 60 * 1_000;

/// Parsed rules for one user agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotsRules {
    /// `Crawl-delay` in seconds, when the site declares one.
    pub crawl_delay_s: Option<u64>,
    /// Path patterns that may not be fetched.
    pub disallow: Vec<String>,
    /// Path patterns explicitly permitted, which override a broader `Disallow`.
    pub allow: Vec<String>,
    /// Sitemap URLs the file points at.
    ///
    /// Not a rule and not scoped to a user agent: `Sitemap` is a property of the
    /// file as a whole, which is why it is collected from every line regardless
    /// of the group it sits in. It rides along on the parsed rules so that the
    /// sitemap list is cached by the same lookup, with the same lifetime, as the
    /// rules beside it.
    #[serde(default)]
    pub sitemaps: Vec<String>,
}

impl RobotsRules {
    /// Rules that permit everything, for hosts with no robots.txt at all.
    ///
    /// Note that a *missing* robots.txt permits crawling, while an unreachable
    /// one does not mean the same thing: RFC 9309 says an unavailable robots.txt
    /// should be treated as a full disallow for a short period.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Rules that permit nothing.
    ///
    /// The conservative reading of RFC 9309 for a robots.txt that could not be
    /// fetched at all: a 5xx may be a failing origin under load or a rate limit,
    /// and the safe answer to "I do not know what this site allows" is to fetch
    /// nothing until we do.
    pub fn disallow_all() -> Self {
        Self {
            disallow: vec!["/".to_string()],
            ..Self::default()
        }
    }

    /// Whether `path` may be fetched.
    ///
    /// Longest matching pattern wins; `Allow` wins a tie. With no matching rule
    /// the path is permitted, which is what makes an empty robots.txt a no-op.
    pub fn allows(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None;

        for pattern in &self.disallow {
            if let Some(specificity) = match_path(pattern, path) {
                consider(&mut best, specificity, false);
            }
        }
        for pattern in &self.allow {
            if let Some(specificity) = match_path(pattern, path) {
                consider(&mut best, specificity, true);
            }
        }

        best.map(|(_, allowed)| allowed).unwrap_or(true)
    }
}

/// Keeps the most specific rule seen so far.
fn consider(best: &mut Option<(usize, bool)>, specificity: usize, allowed: bool) {
    match best {
        None => *best = Some((specificity, allowed)),
        // Strictly greater: on a tie the earlier, `Disallow`, rule stands, which
        // is the conservative reading.
        Some((current, _)) if specificity > *current => *best = Some((specificity, allowed)),
        _ => {}
    }
}

/// Returns the pattern's specificity if it matches `path`.
fn match_path(pattern: &str, path: &str) -> Option<usize> {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(stripped) => (stripped, true),
        None => (pattern, false),
    };

    if glob_matches(pattern, path, anchored) {
        Some(pattern.len())
    } else {
        None
    }
}

/// Glob match supporting `*`, with robots.txt's prefix semantics.
///
/// Without `$` the pattern only has to match a prefix of the path, which is how
/// `Disallow: /private` comes to cover `/private/x`. With `$` it must match the
/// whole path.
fn glob_matches(pattern: &str, text: &str, anchored: bool) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();

    let (mut pattern_at, mut text_at) = (0usize, 0usize);
    let mut star_at: Option<usize> = None;
    let mut resume_at = 0usize;

    loop {
        if pattern_at < pattern.len() && pattern[pattern_at] == '*' {
            star_at = Some(pattern_at);
            resume_at = text_at;
            pattern_at += 1;
        } else if pattern_at < pattern.len()
            && text_at < text.len()
            && pattern[pattern_at] == text[text_at]
        {
            pattern_at += 1;
            text_at += 1;
        } else if pattern_at == pattern.len() {
            // Pattern exhausted. Anchored patterns must have consumed the whole
            // path; otherwise matching a prefix is exactly the intent.
            return !anchored || text_at == text.len();
        } else if let Some(star) = star_at {
            resume_at += 1;
            if resume_at > text.len() {
                return false;
            }
            pattern_at = star + 1;
            text_at = resume_at;
        } else {
            return false;
        }
    }
}

/// The robots.txt URL for the origin a page was reached on.
///
/// Scheme and port are taken from the page rather than assumed. Asking an
/// `http://` origin for `https://host:443/robots.txt` is a request the site never
/// offered, it can never succeed, and paying for it on every page is what made a
/// crawl of a host with no robots.txt run at a fraction of its speed.
pub fn robots_url(page_url: &str) -> Option<String> {
    let mut parsed = url::Url::parse(page_url).ok()?;
    parsed.set_path("/robots.txt");
    parsed.set_query(None);
    parsed.set_fragment(None);
    Some(parsed.to_string())
}

/// Parses a robots.txt body for a given user agent.
pub fn parse(body: &str, user_agent: &str) -> RobotsRules {
    let mut groups: Vec<(Vec<String>, RobotsRules)> = Vec::new();
    let mut agents: Vec<String> = Vec::new();
    let mut rules = RobotsRules::default();
    let mut group_has_rules = false;
    let mut sitemaps: Vec<String> = Vec::new();

    for line in body.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let field = field.trim().to_ascii_lowercase();
        let value = value.trim();

        match field.as_str() {
            "user-agent" => {
                // A new user-agent line after rules starts a new group.
                if group_has_rules {
                    groups.push((std::mem::take(&mut agents), std::mem::take(&mut rules)));
                    group_has_rules = false;
                }
                agents.push(value.to_ascii_lowercase());
            }
            "disallow" => {
                group_has_rules = true;
                // An empty Disallow means "no restriction", not "block nothing by
                // this rule"; storing it would match every path.
                if !value.is_empty() {
                    rules.disallow.push(value.to_string());
                }
            }
            "allow" => {
                group_has_rules = true;
                if !value.is_empty() {
                    rules.allow.push(value.to_string());
                }
            }
            "crawl-delay" => {
                group_has_rules = true;
                if let Ok(seconds) = value.parse::<f64>() {
                    if seconds.is_finite() && seconds >= 0.0 {
                        rules.crawl_delay_s = Some(seconds.ceil() as u64);
                    }
                }
            }
            // Deliberately outside the group logic: the field belongs to the
            // file, not to the group, and a parser that only kept the sitemaps of
            // the group it happened to select would miss most of them.
            "sitemap" if !value.is_empty() => {
                sitemaps.push(value.to_string());
            }
            _ => {}
        }
    }

    if group_has_rules && !agents.is_empty() {
        groups.push((agents, rules));
    }

    let mut selected = select_group(&groups, user_agent)
        .cloned()
        .unwrap_or_default();
    selected.sitemaps = sitemaps;
    selected
}

/// Picks the group whose user agent matches most specifically.
///
/// `*` counts as a match of length zero, so any named group beats it.
fn select_group<'a>(
    groups: &'a [(Vec<String>, RobotsRules)],
    user_agent: &str,
) -> Option<&'a RobotsRules> {
    let user_agent = user_agent.to_ascii_lowercase();
    let mut best: Option<(&RobotsRules, usize)> = None;

    for (agents, rules) in groups {
        for agent in agents {
            let specificity = if agent == "*" {
                0
            } else if user_agent.contains(agent.as_str()) {
                agent.len()
            } else {
                continue;
            };

            if best.is_none_or(|(_, current)| specificity > current) {
                best = Some((rules, specificity));
            }
        }
    }

    best.map(|(rules, _)| rules)
}

/// TTL cache of robots.txt rules, kept in the crawler's database.
pub struct RobotsCache<'a> {
    db: &'a CrawlDb,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    rules: RobotsRules,
    fetched_at_ms: i64,
    /// How long this entry stays fresh. Stored per entry rather than looked up,
    /// because "there is no robots.txt" and "we could not reach robots.txt"
    /// deserve different lifetimes.
    ttl_ms: i64,
}

impl<'a> RobotsCache<'a> {
    /// Wraps the crawler's database.
    pub fn new(db: &'a CrawlDb) -> Self {
        Self { db }
    }

    /// Returns a host's rules when the cached copy is still fresh.
    ///
    /// An entry that cannot be decoded is reported as absent rather than as an
    /// error. The cache is a cache: a value written by an older build, or one a
    /// crash left half-written, is not a reason to fail a page that could simply
    /// be fetched again.
    pub fn get(&self, host: &str, now_ms: i64) -> Result<Option<RobotsRules>, CrawlError> {
        let Some(bytes) = self
            .db
            .db()
            .get_cf(self.db.cf(CF_ROBOTS)?, host.as_bytes())?
        else {
            return Ok(None);
        };

        let entry: Entry = match postcard::from_bytes(&bytes) {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(
                    host = %host,
                    error = %error,
                    "discarding an unreadable robots.txt cache entry"
                );
                return Ok(None);
            }
        };
        if now_ms.saturating_sub(entry.fetched_at_ms) > entry.ttl_ms {
            return Ok(None);
        }
        Ok(Some(entry.rules))
    }

    /// Stores a host's freshly fetched rules with the standard lifetime.
    pub fn put(&self, host: &str, rules: &RobotsRules, now_ms: i64) -> Result<(), CrawlError> {
        self.put_for(host, rules, now_ms, CACHE_TTL_MS)
    }

    /// Stores a host's rules with an explicit lifetime.
    pub fn put_for(
        &self,
        host: &str,
        rules: &RobotsRules,
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<(), CrawlError> {
        let entry = Entry {
            rules: rules.clone(),
            fetched_at_ms: now_ms,
            ttl_ms,
        };
        let bytes = postcard::to_allocvec(&entry).map_err(CrawlError::CacheEncode)?;
        self.db
            .db()
            .put_cf(self.db.cf(CF_ROBOTS)?, host.as_bytes(), bytes)?;
        Ok(())
    }

    /// Number of hosts with cached rules.
    pub fn len(&self) -> Result<u64, CrawlError> {
        self.db.count_estimate(CF_ROBOTS)
    }

    /// Whether no host has cached rules.
    pub fn is_empty(&self) -> Result<bool, CrawlError> {
        Ok(self.len()? == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UA: &str = "MJLsearchBot";

    #[test]
    fn an_empty_file_permits_everything() {
        let rules = parse("", UA);
        assert!(rules.allows("/anything"));
        assert_eq!(rules.crawl_delay_s, None);
    }

    #[test]
    fn an_empty_disallow_means_no_restriction() {
        // The classic trap: treating `Disallow:` as a pattern would match every
        // path and block the whole site.
        let rules = parse("User-agent: *\nDisallow:\n", UA);
        assert!(rules.disallow.is_empty());
        assert!(rules.allows("/anything"));
    }

    #[test]
    fn a_disallow_is_a_path_prefix() {
        let rules = parse("User-agent: *\nDisallow: /private\n", UA);
        assert!(!rules.allows("/private"));
        assert!(!rules.allows("/private/page"));
        assert!(rules.allows("/public"));
    }

    #[test]
    fn matching_is_by_prefix_not_by_path_segment() {
        // This is the trap that makes `Disallow: /private` cover `/privateer` as
        // well. It is the correct RFC 9309 reading, and it is exactly why sites
        // that mean a directory should write `Disallow: /private/`.
        let rules = parse("User-agent: *\nDisallow: /private\n", UA);
        assert!(!rules.allows("/privateer"));

        let trailing_slash = parse("User-agent: *\nDisallow: /private/\n", UA);
        assert!(!trailing_slash.allows("/private/page"));
        assert!(trailing_slash.allows("/privateer"));
    }

    #[test]
    fn the_longest_matching_rule_wins() {
        let rules = parse("User-agent: *\nDisallow: /dir\nAllow: /dir/public\n", UA);
        assert!(!rules.allows("/dir/secret"));
        assert!(rules.allows("/dir/public/page"));
    }

    #[test]
    fn a_wildcard_matches_in_the_middle() {
        let rules = parse("User-agent: *\nDisallow: /*.pdf\n", UA);
        assert!(!rules.allows("/papers/report.pdf"));
        assert!(!rules.allows("/a/b/c.pdf"));
        assert!(rules.allows("/papers/report.html"));
    }

    #[test]
    fn a_dollar_anchor_requires_the_whole_path() {
        let rules = parse("User-agent: *\nDisallow: /*.php$\n", UA);
        assert!(!rules.allows("/index.php"));
        // The anchor means the query string is not covered.
        assert!(rules.allows("/index.php?page=2"));
    }

    #[test]
    fn query_strings_are_within_the_path() {
        let rules = parse("User-agent: *\nDisallow: /search?\n", UA);
        assert!(!rules.allows("/search?q=x"));
        assert!(rules.allows("/search/deep"));
    }

    #[test]
    fn a_named_group_beats_the_wildcard_group() {
        let body = "\
User-agent: *\n\
Disallow: /\n\
\n\
User-agent: MJLsearchBot\n\
Disallow: /admin\n\
Crawl-delay: 2\n";
        let rules = parse(body, UA);

        assert_eq!(rules.crawl_delay_s, Some(2));
        assert!(!rules.allows("/admin"));
        // The wildcard group's full disallow must not leak into our group.
        assert!(rules.allows("/everything-else"));
    }

    #[test]
    fn an_unrelated_named_group_is_ignored() {
        let body = "User-agent: SomeoneElseBot\nDisallow: /\n";
        let rules = parse(body, UA);
        assert!(rules.allows("/anything"));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let body = "\
# a comment\n\
\n\
User-agent: *   # inline comment\n\
Disallow: /secret  # why\n";
        let rules = parse(body, UA);
        assert!(!rules.allows("/secret"));
        assert_eq!(rules.disallow, vec!["/secret".to_string()]);
    }

    #[test]
    fn a_fractional_crawl_delay_rounds_up() {
        // Rounding down would make us faster than the site asked for.
        let rules = parse("User-agent: *\nCrawl-delay: 1.2\n", UA);
        assert_eq!(rules.crawl_delay_s, Some(2));
    }

    #[test]
    fn a_nonsense_crawl_delay_is_ignored_rather_than_fatal() {
        let rules = parse("User-agent: *\nCrawl-delay: soon\n", UA);
        assert_eq!(rules.crawl_delay_s, None);
        let rules = parse("User-agent: *\nCrawl-delay: -5\n", UA);
        assert_eq!(rules.crawl_delay_s, None);
    }

    #[test]
    fn a_line_without_a_colon_is_skipped() {
        let rules = parse("garbage\nUser-agent: *\nDisallow: /x\n", UA);
        assert!(!rules.allows("/x"));
    }

    #[test]
    fn the_glob_matcher_handles_repeated_wildcards() {
        assert!(glob_matches("a*b*c", "aXbYc", false));
        assert!(glob_matches("*", "anything", false));
        assert!(glob_matches("*", "", false));
        assert!(!glob_matches("a*b", "a", false));
        assert!(glob_matches("", "/anything", false));
    }

    #[test]
    fn cached_rules_are_returned_until_they_expire() {
        let directory = tempfile::tempdir().expect("tempdir");
        let db = CrawlDb::open(directory.path(), &crate::db::DbConfig::default()).expect("open");
        let cache = RobotsCache::new(&db);

        let rules = parse("User-agent: *\nDisallow: /x\n", UA);
        cache.put("example.com", &rules, 1_000).expect("put");

        assert!(cache.get("example.com", 1_000).expect("get").is_some());
        assert!(cache.get("example.com", 9_000_000).expect("get").is_some());
        // Past the 24h TTL the entry is treated as absent, so it gets refetched.
        assert!(
            cache
                .get("example.com", 1_000 + CACHE_TTL_MS + 1)
                .expect("get")
                .is_none()
        );
        assert!(cache.get("other.example", 1_000).expect("get").is_none());
    }

    #[test]
    fn the_cache_ttl_is_one_day() {
        assert_eq!(CACHE_TTL_MS, 86_400_000);
    }

    #[test]
    fn sitemap_urls_are_collected_from_any_group() {
        // The directive belongs to the file, not to a user-agent group. Reading
        // only the selected group would find none of these.
        let body = "\
Sitemap: https://example.com/sitemap.xml\n\
User-agent: *\n\
Disallow: /private\n\
Sitemap: https://example.com/news.xml\n";
        let rules = parse(body, UA);

        assert_eq!(
            rules.sitemaps,
            vec![
                "https://example.com/sitemap.xml".to_string(),
                "https://example.com/news.xml".to_string()
            ]
        );
        // ...and the rules themselves are still the right group's.
        assert!(!rules.allows("/private"));
    }

    #[test]
    fn a_file_without_sitemaps_says_so() {
        assert!(
            parse("User-agent: *\nDisallow: /x\n", UA)
                .sitemaps
                .is_empty()
        );
    }

    #[test]
    fn disallow_all_blocks_every_path() {
        let rules = RobotsRules::disallow_all();
        assert!(!rules.allows("/"));
        assert!(!rules.allows("/anything/at/all"));
        assert!(!rules.allows("/robots.txt"));
    }

    #[test]
    fn an_unreadable_cache_entry_is_treated_as_a_miss() {
        // This is what a schema change or a crash mid-write leaves behind, and a
        // crawl must refetch rather than fail.
        let directory = tempfile::tempdir().expect("tempdir");
        let db = CrawlDb::open(directory.path(), &crate::db::DbConfig::default()).expect("open");
        let cache = RobotsCache::new(&db);

        db.db()
            .put_cf(
                db.cf(CF_ROBOTS).expect("cf"),
                b"example.com",
                b"\x00 not postcard",
            )
            .expect("put garbage");

        assert!(cache.get("example.com", 1_000).expect("get").is_none());
    }

    #[test]
    fn the_robots_url_keeps_the_origin_scheme_and_port() {
        // The bug this pins: hardcoding `https://` sent an `http://`-only origin a
        // request on port 443 that could never succeed, and because the failure was
        // never cached, every page of the crawl paid for it.
        assert_eq!(
            robots_url("http://127.0.0.1:8098/p10.html").as_deref(),
            Some("http://127.0.0.1:8098/robots.txt")
        );
        assert_eq!(
            robots_url("https://example.com/a/b?q=1#frag").as_deref(),
            Some("https://example.com/robots.txt")
        );
        assert_eq!(robots_url("not a url"), None);
    }

    #[test]
    fn an_unreachable_robots_txt_is_cached_far_more_briefly_than_a_fetched_one() {
        let directory = tempfile::tempdir().expect("tempdir");
        let db = CrawlDb::open(directory.path(), &crate::db::DbConfig::default()).expect("open");
        let cache = RobotsCache::new(&db);

        cache
            .put_for(
                "example.com",
                &RobotsRules::allow_all(),
                0,
                UNAVAILABLE_TTL_MS,
            )
            .expect("put");

        // A cached failure still keeps us from retrying on the very next page...
        assert!(cache.get("example.com", 1_000).expect("get").is_some());
        // ...but it is forgotten within minutes, not a day.
        assert!(
            cache
                .get("example.com", UNAVAILABLE_TTL_MS + 1)
                .expect("get")
                .is_none()
        );
    }
}
