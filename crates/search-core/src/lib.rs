//! Index and query engine for the search engine.
//!
//! The crate is used in two ways:
//!
//! * as a `cdylib` loaded into the Python API process, where the `python`
//!   feature turns on the PyO3 bindings, and
//! * as an `rlib` linked by the `indexer` binary, which is the only process that
//!   may write to the index.
//!
//! Both users share this one implementation of the schema and the writer, so
//! there is no second copy to drift out of step.
#![forbid(unsafe_code)]

pub mod index_writer;
pub mod schema;
pub mod search;

pub use index_writer::{DocScores, DocWriter, IndexWriterConfig};
pub use schema::IndexSchema;
pub use search::{Hit, SearchEngine, SearchError, SearchOutcome};

#[cfg(feature = "python")]
mod python_api;
