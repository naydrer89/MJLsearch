//! Types shared across every stage of the pipeline.
//!
//! The crawler produces [`Document`]s and seals them into spool segments, the
//! indexer drains those segments into Tantivy, and search-core reads the
//! resulting index. Keeping one definition of the document and one definition of
//! the hand-off format here is what stops the two sides from drifting apart
//! silently.
//!
//! This crate is intentionally free of heavyweight dependencies. It is linked
//! into `search_core`, which is itself loaded into the Python interpreter, so
//! every dependency added here is paid for by the API process. Notably it does
//! **not** depend on RocksDB: that database is used by the crawler alone, and
//! only the crawler pays for it.
#![forbid(unsafe_code)]

pub mod document;
pub mod spool;

pub use document::{Document, content_hash};
