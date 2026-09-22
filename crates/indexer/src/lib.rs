//! Drains crawled documents into the search index.
//!
//! This binary exists because the crawler must not write the index. Tantivy
//! permits exactly one writer process per index, so ownership has to sit
//! somewhere, and putting it here buys three things:
//!
//! * a slow commit no longer stalls the fetch loop;
//! * the crawler never links Tantivy, and this binary never links an HTTP client
//!   or a TLS stack;
//! * reindexing stays a CLI operation, so the read-only API cannot be turned into
//!   a write path by a request.
//!
//! The writer itself is [`search_core::DocWriter`], shared with the Python
//! extension rather than reimplemented, so the schema and the commit policy have
//! exactly one definition.
#![forbid(unsafe_code)]

pub mod drain;
pub mod graph;
pub mod history;
pub mod spool;

pub use drain::{DrainConfig, DrainReport, run};
pub use graph::{GraphLimits, LinkGraph};
pub use history::IndexHistory;
pub use spool::{for_each_document, retire, sealed_segments};

/// Errors raised by the indexer.
#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// Local file I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The index rejected a write.
    #[error("index error: {0}")]
    Index(#[from] search_core::SearchError),
    /// A spool segment could not be read.
    #[error("spool error: {0}")]
    Spool(#[from] common::spool::SpoolError),
}
