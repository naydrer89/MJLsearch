//! Fetching, with per-host concurrency limits and retry.
//!
//! Invariants that hold here:
//!
//! * every request is bounded by [`FetchConfig::timeout`];
//! * the body is read in chunks against a hard cap, so one enormous response
//!   cannot pull the crawler's memory footprint up with it;
//! * retries use [`backoff_ms`] plus jitter;
//! * failure is a returned value, never a panic, so one unreachable page cannot
//!   take the crawl down.
//!
//! Per-host concurrency is *not* enforced here. It is enforced by the crawl loop,
//! which knows how many requests are outstanding per host and simply declines to
//! hand out more; a semaphore inside the fetcher would have to be shared across
//! the whole process to be meaningful, and the loop is already the single place
//! that owns that state.

use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, HeaderMap, RETRY_AFTER};

use crate::CrawlError;

/// Fetch policy.
#[derive(Debug, Clone)]
pub struct FetchConfig {
    /// Wall-clock budget for a single attempt.
    pub timeout: Duration,
    /// Budget for establishing the connection alone.
    pub connect_timeout: Duration,
    /// Additional attempts after the first failure.
    pub max_retries: u32,
    /// Simultaneous requests permitted against one host, used for pool sizing.
    pub max_concurrent_per_host: usize,
    /// `User-Agent` sent with every request.
    pub user_agent: String,
    /// First retry delay, doubled per attempt.
    pub backoff_base_ms: u64,
    /// Ceiling on the retry delay.
    pub backoff_cap_ms: u64,
    /// Largest response body we will read.
    pub max_body_bytes: usize,
    /// `Accept` header. Servers negotiate on this, and a client that omits it is
    /// often handed something other than the page.
    pub accept: String,
    /// `Accept-Language` header.
    pub accept_language: String,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(5),
            max_retries: 3,
            max_concurrent_per_host: 2,
            user_agent: concat!("MJLsearchBot/", env!("CARGO_PKG_VERSION")).to_string(),
            backoff_base_ms: 250,
            backoff_cap_ms: 30_000,
            // 8 MiB: comfortably above the largest real HTML documents, and small
            // enough that a handful of concurrent fetches cannot dominate RAM.
            max_body_bytes: 8 * 1024 * 1024,
            accept: "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".to_string(),
            accept_language: "en;q=0.9,*;q=0.5".to_string(),
        }
    }
}

/// The two bytes every gzip stream starts with.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// A retrieved page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedPage {
    /// URL after redirects, and the URL to use as the document's canonical URL.
    pub final_url: String,
    /// HTTP status code.
    pub status: u16,
    /// `Content-Type` header, used to reject non-HTML bodies before parsing.
    pub content_type: Option<String>,
    /// Response body, decoded with UTF-8 replacement.
    pub body: String,
}

/// Exponential backoff in milliseconds for a zero-based retry attempt.
///
/// Deliberately deterministic, so it can be tested; the caller adds jitter.
/// Without jitter every worker retrying the same failing host wakes at the same
/// instant and the retry storm is worse than the original failure.
pub fn backoff_ms(attempt: u32, base_ms: u64, cap_ms: u64) -> u64 {
    let shift = attempt.min(63);
    base_ms
        .saturating_mul(1u64.checked_shl(shift).unwrap_or(u64::MAX))
        .min(cap_ms)
}

/// Extra delay, in milliseconds, to spread out simultaneous retries.
///
/// Derived from the clock as well as the URL so that two workers backing off from
/// the same host do not converge on the same instant.
pub fn jitter_ms(url: &str, attempt: u32, nanos: u64) -> u64 {
    common::content_hash(&format!("{url}:{attempt}:{nanos}")) % 250
}

/// Whether an HTTP status is worth retrying.
///
/// 429 and 5xx are transient. Every other 4xx is a statement about the request
/// itself, so retrying it just wastes the host's patience.
pub fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Whether a `Content-Type` looks like something worth parsing.
pub fn is_html_content_type(content_type: Option<&str>) -> bool {
    match content_type {
        None => true,
        Some(value) => {
            let value = value.to_ascii_lowercase();
            value.contains("html") || value.contains("xml")
        }
    }
}

/// Parses a `Retry-After` header into milliseconds.
///
/// Only the delta-seconds form is honoured. The other legal form is an HTTP
/// date, which would need a date parser and a clock with the right skew to be
/// safe; rate limiters send seconds in practice, and a date we cannot read
/// simply falls back to our own exponential backoff rather than being guessed
/// at.
pub fn parse_retry_after(value: &str) -> Option<u64> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(|s| s.saturating_mul(1_000))
}

/// The `Retry-After` delay a response asks for, if it states one.
fn retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()
        .and_then(parse_retry_after)
}

/// The `charset` parameter of a `Content-Type`, as written.
pub fn charset_of(content_type: Option<&str>) -> Option<String> {
    let value = content_type?;
    let lowered = value.to_ascii_lowercase();
    let start = lowered.find("charset=")? + "charset=".len();

    let rest = &value[start..];
    let end = rest.find([';', ' ', '\t']).unwrap_or(rest.len());
    let label = rest[..end].trim().trim_matches(['"', '\'']);

    if label.is_empty() {
        None
    } else {
        Some(label.to_ascii_lowercase())
    }
}

/// A failure, tagged with whether retrying could plausibly help.
struct Failure {
    error: CrawlError,
    retryable: bool,
    /// Delay the server itself asked for, in milliseconds.
    retry_after_ms: Option<u64>,
}

impl Failure {
    fn terminal(error: CrawlError) -> Self {
        Self {
            error,
            retryable: false,
            retry_after_ms: None,
        }
    }

    fn transient(error: CrawlError) -> Self {
        Self {
            error,
            retryable: true,
            retry_after_ms: None,
        }
    }
}

/// An HTTP client configured for crawling.
pub struct Fetcher {
    client: reqwest::Client,
    config: FetchConfig,
}

impl Fetcher {
    /// Builds the client, including its connection pool.
    pub fn new(config: FetchConfig) -> Result<Self, CrawlError> {
        // Headers are built once, here, rather than per request: a malformed one
        // is a configuration mistake that should stop the crawl at startup, not
        // fail a page in the middle of it.
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            config
                .accept
                .parse()
                .map_err(|_| CrawlError::InvalidHeader {
                    name: "Accept",
                    value: config.accept.clone(),
                })?,
        );
        headers.insert(
            ACCEPT_LANGUAGE,
            config
                .accept_language
                .parse()
                .map_err(|_| CrawlError::InvalidHeader {
                    name: "Accept-Language",
                    value: config.accept_language.clone(),
                })?,
        );

        let client = reqwest::Client::builder()
            .user_agent(config.user_agent.clone())
            .default_headers(headers)
            .timeout(config.timeout)
            .connect_timeout(config.connect_timeout)
            // Bounded redirects: an open redirect chain is otherwise an easy way
            // to make one URL consume unbounded time.
            .redirect(reqwest::redirect::Policy::limited(5))
            .pool_max_idle_per_host(config.max_concurrent_per_host)
            .build()?;

        Ok(Self { client, config })
    }

    /// The policy in force.
    pub fn config(&self) -> &FetchConfig {
        &self.config
    }

    /// Fetches one URL, retrying transient failures with backoff.
    ///
    /// Non-HTML responses are refused, because indexing them would be wasted work
    /// and buffering them would be wasted memory.
    pub async fn fetch(&self, url: &str) -> Result<FetchedPage, CrawlError> {
        self.fetch_inner(url, true).await
    }

    /// Fetches one URL without the HTML-only restriction.
    ///
    /// Used for `robots.txt`, which is `text/plain` and would otherwise be
    /// rejected by [`Fetcher::fetch`]'s content-type check.
    pub async fn fetch_text(&self, url: &str) -> Result<FetchedPage, CrawlError> {
        self.fetch_inner(url, false).await
    }

    async fn fetch_inner(&self, url: &str, require_html: bool) -> Result<FetchedPage, CrawlError> {
        let mut attempt = 0u32;

        loop {
            match self.attempt(url, require_html).await {
                Ok(page) => return Ok(page),
                Err(failure) => {
                    if !failure.retryable || attempt >= self.config.max_retries {
                        return Err(failure.error);
                    }
                    // A server that names a delay longer than our ceiling is
                    // telling us it does not want us back during this run. Waiting
                    // it out would stall a worker for minutes, and ignoring it
                    // would be rude, so the page is dropped instead.
                    if let Some(requested) = failure.retry_after_ms {
                        if requested > self.config.backoff_cap_ms {
                            tracing::debug!(
                                url = %url,
                                retry_after_ms = requested,
                                "server asked for a longer pause than this crawl allows"
                            );
                            return Err(failure.error);
                        }
                    }

                    let backoff = backoff_ms(
                        attempt,
                        self.config.backoff_base_ms,
                        self.config.backoff_cap_ms,
                    ) + jitter_ms(url, attempt, now_nanos());
                    // `Retry-After` is a floor, not a replacement: it can only
                    // ever make us wait longer than our own backoff would have.
                    let delay = failure.retry_after_ms.unwrap_or(0).max(backoff);

                    tracing::debug!(
                        url = %url,
                        attempt,
                        delay_ms = delay,
                        error = %failure.error,
                        "retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn attempt(&self, url: &str, require_html: bool) -> Result<FetchedPage, Failure> {
        let response = self.client.get(url).send().await.map_err(|error| {
            // A timeout or a connection reset is worth another go; a malformed
            // URL or a TLS policy failure is not.
            if error.is_timeout() || error.is_connect() || error.is_request() {
                Failure::transient(CrawlError::http(error))
            } else {
                Failure::terminal(CrawlError::http(error))
            }
        })?;

        let status = response.status();
        if !status.is_success() {
            let error = CrawlError::HttpStatus(status.as_u16());
            return Err(if is_retryable_status(status.as_u16()) {
                Failure {
                    error,
                    retryable: true,
                    retry_after_ms: retry_after_ms(response.headers()),
                }
            } else {
                Failure::terminal(error)
            });
        }

        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);

        if require_html && !is_html_content_type(content_type.as_deref()) {
            return Err(Failure::terminal(CrawlError::UnsupportedContent(
                content_type.unwrap_or_default(),
            )));
        }

        // A declared length beyond the cap is refused before a single byte is
        // buffered.
        if let Some(length) = response.content_length() {
            if length > self.config.max_body_bytes as u64 {
                return Err(Failure::terminal(CrawlError::BodyTooLarge {
                    limit: self.config.max_body_bytes,
                }));
            }
        }

        let mut body = Vec::new();
        let mut response = response;
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > self.config.max_body_bytes {
                        return Err(Failure::terminal(CrawlError::BodyTooLarge {
                            limit: self.config.max_body_bytes,
                        }));
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(error) => {
                    // The body was interrupted mid-stream: the page is incomplete,
                    // so it is worth retrying rather than indexing half a document.
                    return Err(Failure::transient(CrawlError::http(error)));
                }
            }
        }

        let body = self.decode(body, &content_type);

        Ok(FetchedPage {
            final_url,
            status: status.as_u16(),
            content_type,
            body,
        })
    }

    /// Turns a response body into text.
    ///
    /// Two things happen here that a plain UTF-8 conversion gets wrong on real
    /// sites. A body is decoded with the encoding the response declares, because
    /// a page served as `ISO-8859-1` would otherwise be indexed full of
    /// replacement characters. And a sitemap is very often a literal gzip file
    /// (`sitemap.xml.gz` is the convention), which arrives as compressed bytes
    /// with no `Content-Encoding` header to trigger transparent decoding.
    fn decode(&self, body: Vec<u8>, content_type: &Option<String>) -> String {
        let body = gunzip_if_compressed(body, self.config.max_body_bytes);

        let Some(label) = charset_of(content_type.as_deref()) else {
            return String::from_utf8_lossy(&body).into_owned();
        };
        // UTF-8 and its ASCII subset need no transcoding: the lossy conversion
        // below is the same result for a fraction of the work.
        if matches!(label.as_str(), "utf-8" | "utf8" | "us-ascii" | "ascii") {
            return String::from_utf8_lossy(&body).into_owned();
        }

        match encoding_rs::Encoding::for_label(label.as_bytes()) {
            Some(encoding) => encoding.decode(&body).0.into_owned(),
            // An unknown label is not a reason to lose the page.
            None => String::from_utf8_lossy(&body).into_owned(),
        }
    }
}

/// Decompresses a gzip body, bounded by `cap` bytes of *output*.
///
/// The bound is on the decompressed size on purpose: a small compressed payload
/// that expands to gigabytes is the oldest denial of service there is, and a
/// limit on the compressed side would not stop it.
fn gunzip_if_compressed(body: Vec<u8>, cap: usize) -> Vec<u8> {
    if !body.starts_with(&GZIP_MAGIC) {
        return body;
    }

    let mut expanded = Vec::new();
    {
        let mut reader = flate2::read::GzDecoder::new(&body[..]).take(cap as u64);
        if reader.read_to_end(&mut expanded).is_ok() {
            return expanded;
        }
    }

    // Not a gzip stream after all, or a truncated one: the raw bytes are still
    // the best answer available.
    body
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_exponentially_then_flattens_at_the_cap() {
        assert_eq!(backoff_ms(0, 100, 60_000), 100);
        assert_eq!(backoff_ms(1, 100, 60_000), 200);
        assert_eq!(backoff_ms(2, 100, 60_000), 400);
        assert_eq!(backoff_ms(3, 100, 60_000), 800);
        assert_eq!(backoff_ms(20, 100, 60_000), 60_000);
    }

    #[test]
    fn backoff_saturates_instead_of_overflowing() {
        // A shift this large would overflow if computed unchecked, and an
        // overflow panic in the retry path would abort the crawl.
        assert_eq!(backoff_ms(64, 100, 60_000), 60_000);
        assert_eq!(backoff_ms(u32::MAX, 100, 60_000), 60_000);
    }

    #[test]
    fn jitter_stays_within_its_budget_and_is_stable_for_the_same_inputs() {
        for attempt in 0..5 {
            let value = jitter_ms("https://example.com/a", attempt, 12345);
            assert!(value < 250, "jitter {value} should be under the budget");
            assert_eq!(value, jitter_ms("https://example.com/a", attempt, 12345));
        }
    }

    #[test]
    fn jitter_differs_between_nanosecond_inputs() {
        // The whole point: two workers backing off from the same host must not
        // wake at the same instant.
        let first = jitter_ms("https://example.com/a", 1, 1);
        let second = jitter_ms("https://example.com/a", 1, 999_999);
        assert_ne!(first, second);
    }

    #[test]
    fn only_transient_statuses_are_retried() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));

        // Retrying these wastes the host's patience for no benefit.
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(410));
        assert!(!is_retryable_status(200));
    }

    #[test]
    fn html_content_types_are_recognised() {
        assert!(is_html_content_type(None));
        assert!(is_html_content_type(Some("text/html; charset=utf-8")));
        assert!(is_html_content_type(Some("application/xhtml+xml")));

        assert!(!is_html_content_type(Some("application/pdf")));
        assert!(!is_html_content_type(Some("image/png")));
        assert!(!is_html_content_type(Some("application/octet-stream")));
    }

    #[test]
    fn default_config_keeps_a_single_host_to_two_connections() {
        let config = FetchConfig::default();
        assert_eq!(config.max_concurrent_per_host, 2);
        assert!(config.timeout > Duration::ZERO);
        assert!(config.user_agent.starts_with("MJLsearchBot/"));
        assert!(config.max_body_bytes <= 16 * 1024 * 1024);
    }

    #[test]
    fn the_client_builds_with_the_configured_user_agent() {
        let fetcher = Fetcher::new(FetchConfig::default()).expect("build client");
        assert_eq!(fetcher.config().max_retries, 3);
    }

    #[test]
    fn a_retry_after_in_seconds_becomes_milliseconds() {
        assert_eq!(parse_retry_after("3"), Some(3_000));
        assert_eq!(parse_retry_after("  30 "), Some(30_000));
        assert_eq!(parse_retry_after("0"), Some(0));
    }

    #[test]
    fn a_retry_after_we_cannot_read_falls_back_to_our_own_backoff() {
        // The HTTP-date form is legal and is not parsed; guessing at a date is
        // worse than waiting our own, tested amount of time.
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("-5"), None);
        assert_eq!(parse_retry_after("soon"), None);
    }

    #[test]
    fn a_retry_after_too_large_to_saturate_does_not_wrap() {
        assert_eq!(parse_retry_after(&u64::MAX.to_string()), Some(u64::MAX));
    }

    #[test]
    fn the_charset_is_read_from_a_content_type() {
        assert_eq!(
            charset_of(Some("text/html; charset=ISO-8859-1")),
            Some("iso-8859-1".to_string())
        );
        assert_eq!(
            charset_of(Some("text/html;charset=utf-8")),
            Some("utf-8".to_string())
        );
        // Quoted and padded spellings both appear in the wild.
        assert_eq!(
            charset_of(Some("text/html; charset=\"windows-1252\"")),
            Some("windows-1252".to_string())
        );
        assert_eq!(charset_of(Some("text/html")), None);
        assert_eq!(charset_of(None), None);
    }

    #[test]
    fn a_declared_non_utf8_body_is_transcoded_rather_than_mangled() {
        let fetcher = Fetcher::new(FetchConfig::default()).expect("build client");
        // 0xFC is `ü` in latin-1 and is not valid UTF-8, so a lossy conversion
        // would produce a replacement character.
        let body = b"Z\xfcrich".to_vec();

        let decoded = fetcher.decode(body.clone(), &Some("text/html; charset=ISO-8859-1".into()));
        assert_eq!(decoded, "Zürich");

        // The same bytes with no charset declared are still readable.
        assert!(!fetcher.decode(body, &None).is_empty());
    }

    #[test]
    fn a_utf8_body_is_not_transcoded() {
        let fetcher = Fetcher::new(FetchConfig::default()).expect("build client");
        let body = "Zürich – 5€".as_bytes().to_vec();
        assert_eq!(
            fetcher.decode(body.clone(), &Some("text/html; charset=utf-8".into())),
            "Zürich – 5€"
        );
        assert_eq!(fetcher.decode(body, &None), "Zürich – 5€");
    }

    #[test]
    fn an_unknown_charset_keeps_the_page() {
        let fetcher = Fetcher::new(FetchConfig::default()).expect("build client");
        let decoded = fetcher.decode(
            b"hello".to_vec(),
            &Some("text/html; charset=nonsense-9".into()),
        );
        assert_eq!(decoded, "hello");
    }

    #[test]
    fn a_gzipped_sitemap_is_expanded_without_a_content_encoding() {
        use std::io::Write;

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(b"<urlset><url><loc>https://example.com/</loc></url></urlset>")
            .expect("write");
        let compressed = encoder.finish().expect("finish");
        assert!(compressed.starts_with(&GZIP_MAGIC));

        let expanded = gunzip_if_compressed(compressed, 1024 * 1024);
        assert!(
            String::from_utf8_lossy(&expanded).contains("https://example.com/"),
            "the payload should have been expanded"
        );
    }

    #[test]
    fn an_expansion_beyond_the_cap_is_bounded() {
        // A tiny compressed payload that expands to a lot of bytes is the oldest
        // denial of service there is, and the limit has to be on the output.
        use std::io::Write;

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder
            .write_all(&vec![b'a'; 4 * 1024 * 1024])
            .expect("write");
        let compressed = encoder.finish().expect("finish");
        assert!(compressed.len() < 64 * 1024, "should compress well");

        let expanded = gunzip_if_compressed(compressed, 4_096);
        assert_eq!(expanded.len(), 4_096);
    }

    #[test]
    fn an_ordinary_body_is_left_alone() {
        let body = b"<html>not compressed</html>".to_vec();
        assert_eq!(gunzip_if_compressed(body.clone(), 1024), body);
    }

    #[test]
    fn a_lying_gzip_magic_falls_back_to_the_raw_bytes() {
        let mut body = GZIP_MAGIC.to_vec();
        body.extend_from_slice(b"this is not actually a gzip stream");
        assert_eq!(gunzip_if_compressed(body.clone(), 1024), body);
    }
}
