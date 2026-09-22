//! Crawler building blocks.
//!
//! The modules here are split so that as much logic as possible is a plain
//! function over plain data: host extraction, politeness arithmetic, backoff,
//! bloom sizing, robots matching and WARC date formatting are all unit-testable
//! without a network or a filesystem, and the I/O-bound parts are kept thin
//! around them.
//!
//! ## What the crawler does not do
//!
//! It never writes the search index. Tantivy permits exactly one writer process
//! per index, and that process is the `indexer` binary. The crawler archives
//! pages to WARC and seals documents into spool segments for it instead. That
//! keeps a slow index commit from stalling the fetch loop, and keeps Tantivy out
//! of the crawler binary.
#![forbid(unsafe_code)]

pub mod db;
pub mod dedup;
pub mod fetcher;
pub mod frontier;
pub mod parser;
pub mod policy;
pub mod robots;
pub mod sitemap;
pub mod storage;

/// Errors raised by the crawler.
///
/// Every fallible operation returns one of these rather than panicking. The
/// per-page loop logs the error, drops the page and continues: a single
/// unreachable or malformed page must never take the crawl down.
#[derive(Debug, thiserror::Error)]
pub enum CrawlError {
    /// The HTTP request failed at the transport layer.
    ///
    /// The message carries the whole `source()` chain rather than reqwest's own
    /// `Display`, which reports only "error sending request for url (...)". The
    /// cause that actually identifies the fault -- a refused connection, a DNS
    /// failure, a TLS verification error -- is one level down, and without it
    /// every transport failure in the log looks identical.
    #[error("http error: {message}")]
    Http {
        /// The error and its causes, joined by `: `.
        message: String,
    },
    /// A key-value store operation failed.
    #[error("backend error: {0}")]
    Backend(#[from] rocksdb::Error),
    /// Local file I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A spool read or write failed.
    #[error("spool error: {0}")]
    Spool(#[from] common::spool::SpoolError),
    /// The URL could not be parsed or is not fetchable.
    #[error("invalid url {url:?}: {reason}")]
    InvalidUrl {
        /// The offending URL.
        url: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// The response was a success but not something we can index.
    #[error("unsupported content type {0:?}")]
    UnsupportedContent(String),
    /// A host's robots.txt says this URL may not be fetched.
    ///
    /// Its own variant because it is not a failure: the site answered exactly as
    /// it was asked to, and lumping it in with broken pages would make a crawl
    /// that is behaving perfectly look like one that is falling apart.
    #[error("disallowed by robots.txt")]
    DisallowedByRobots,
    /// A header the client was configured with could not be built.
    #[error("invalid header {name:?}: {value:?}")]
    InvalidHeader {
        /// Header name.
        name: &'static str,
        /// The value that could not be used.
        value: String,
    },
    /// The HTML stream could not be parsed.
    #[error("html parse error: {0}")]
    Parse(String),
    /// The response body exceeded the configured cap.
    #[error("response body exceeds {limit} bytes")]
    BodyTooLarge {
        /// The cap that was exceeded.
        limit: usize,
    },
    /// The server answered, but not with success.
    ///
    /// Carried as a distinct variant rather than folded into a generic failure
    /// because callers branch on the code: a 404 robots.txt means "no rules",
    /// while a 503 means "come back later".
    #[error("http status {0}")]
    HttpStatus(u16),
    /// A column family was missing from the database that was opened.
    #[error("column family {0:?} is not open")]
    MissingColumnFamily(&'static str),
    /// A cache value could not be decoded.
    #[error("cache decode error: {0}")]
    CacheDecode(#[source] postcard::Error),
    /// A cache value could not be encoded.
    #[error("cache encode error: {0}")]
    CacheEncode(#[source] postcard::Error),
    /// Scaffolding only: this path is not wired up yet.
    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),
}

impl CrawlError {
    /// Wraps a reqwest error together with every cause beneath it.
    pub fn http(error: reqwest::Error) -> Self {
        let mut message = error.to_string();
        let mut source = std::error::Error::source(&error);

        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }

        Self::Http { message }
    }
}

impl From<reqwest::Error> for CrawlError {
    fn from(error: reqwest::Error) -> Self {
        Self::http(error)
    }
}
