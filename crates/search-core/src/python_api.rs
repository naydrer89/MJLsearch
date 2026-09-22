//! PyO3 bindings.
//!
//! This layer is deliberately thin: it owns nothing but the module-level handle
//! to the index, and every piece of real logic lives in a plain Rust module that
//! can be unit-tested without a Python interpreter.
//!
//! The index is opened exactly once, through a [`OnceLock`], and every query
//! afterwards reuses the same mmap. Opening per query would re-map the index on
//! every request and throw away the OS page cache that makes concurrent workers
//! cheap.

use std::path::Path;
use std::sync::OnceLock;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::search::{SearchEngine, SearchError};

/// The process-wide index handle.
static ENGINE: OnceLock<SearchEngine> = OnceLock::new();

fn engine() -> PyResult<&'static SearchEngine> {
    ENGINE.get().ok_or_else(|| {
        PyRuntimeError::new_err(
            "the index is not open yet; call search_core.open_index(path) at startup",
        )
    })
}

/// Returns the version of the compiled extension.
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Opens the index at `index_dir`, once per process, and warms it.
///
/// Idempotent: a second call is a no-op rather than a re-open, so that a
/// preloading import hook or a worker restart cannot end up with two handles.
///
/// The warm-up happens before the handle is published, so no query can slip in
/// and pay the cold-start cost this call is here to absorb.
#[pyfunction]
#[pyo3(signature = (index_dir, cache_entries=None, warm=true))]
fn open_index(index_dir: &str, cache_entries: Option<u64>, warm: bool) -> PyResult<bool> {
    if ENGINE.get().is_some() {
        return Ok(false);
    }

    let engine = match cache_entries {
        Some(capacity) => SearchEngine::open_with_cache(Path::new(index_dir), capacity),
        None => SearchEngine::open(Path::new(index_dir)),
    }
    .map_err(PyErr::from)?;

    // `warm` is a switch rather than an unconditional step so the trade-off is
    // measurable: it moves the cold-mmap cost from the first caller to start-up,
    // which is the right call for a server and arguably the wrong one for a
    // one-shot script.
    if warm {
        match engine.warm_up() {
            Ok(elapsed) => tracing::info!(
                index_dir = index_dir,
                doc_count = engine.doc_count().unwrap_or(0),
                warm_ms = elapsed.as_secs_f64() * 1_000.0,
                "index warmed"
            ),
            // A warm-up that fails is not a reason to refuse to serve: the index
            // is open, and the first query will simply be slower.
            Err(error) => tracing::warn!(error = %error, "index warm-up failed"),
        }
    }

    // If another thread won the race, the handle it installed is equivalent and
    // is kept; there is nothing to unwind.
    Ok(ENGINE.set(engine).is_ok())
}

/// Whether the index handle has been opened in this process.
#[pyfunction]
fn is_open() -> bool {
    ENGINE.get().is_some()
}

/// Runs a query, returning a dict of results and metadata.
///
/// The return shape is a dict rather than a bare list of hits, which is a
/// deliberate deviation from the original interface sketch: `total` and
/// `took_ms` are both part of the API's response contract, and a bare list
/// cannot carry them without running the query a second time.
#[pyfunction]
#[pyo3(signature = (query, limit=10, offset=0, request_id=None))]
fn search<'py>(
    py: Python<'py>,
    query: &str,
    limit: usize,
    offset: usize,
    request_id: Option<&str>,
) -> PyResult<Bound<'py, PyDict>> {
    let engine = engine()?;

    // The same identifier the API logged with, so one request can be traced
    // across the PyO3 boundary.
    tracing::debug!(
        request_id = request_id.unwrap_or("-"),
        query = %query,
        limit,
        offset,
        "search_core query"
    );

    let outcome = engine.search(query, limit, offset).map_err(PyErr::from)?;

    let response = PyDict::new(py);
    response.set_item("query", query)?;
    response.set_item("total_matches", outcome.total_matches)?;
    response.set_item("doc_count", outcome.doc_count)?;
    response.set_item("took_ms", outcome.elapsed_ms)?;
    response.set_item("cached", outcome.cached)?;
    response.set_item("relaxed", outcome.relaxed)?;
    response.set_item("limit", limit)?;
    response.set_item("offset", offset)?;

    let results = PyList::empty(py);
    for hit in &outcome.hits {
        let item = PyDict::new(py);
        item.set_item("url", &hit.url)?;
        item.set_item("title", &hit.title)?;
        item.set_item("snippet", &hit.snippet)?;
        item.set_item("fetched_at", hit.fetched_at)?;
        item.set_item("score", hit.score)?;
        // The link-graph scores, handed to Python so the ranking policy can weigh
        // them. Crossing the boundary as plain numbers keeps the policy testable
        // in Python without a compiled extension.
        item.set_item("authority", hit.authority)?;
        item.set_item("host_authority", hit.host_authority)?;
        results.append(item)?;
    }
    response.set_item("results", results)?;

    Ok(response)
}

/// Read-only statistics about the open index.
///
/// `fingerprint` is a hash of the current segment set, not a commit counter: it
/// changes only when a commit actually alters the index, which is what makes it
/// usable both as a cache stamp and as a "has anything changed" signal.
#[pyfunction]
fn index_stats(py: Python<'_>) -> PyResult<Bound<'_, PyDict>> {
    let engine = engine()?;
    // One reload for both numbers: they are read as a pair so a caller can tell whether
    // anything changed, and a pair taken across two generations answers that wrongly. It
    // is also most of the cost of this call -- the dashboard asks every two seconds.
    let (doc_count, fingerprint) = engine.stats()?;
    let stats = PyDict::new(py);
    stats.set_item("doc_count", doc_count)?;
    stats.set_item("fingerprint", fingerprint)?;
    stats.set_item("cache_entries", engine.cache_entries())?;
    Ok(stats)
}

/// Maps engine errors onto Python exception types.
///
/// A bad query is the caller's mistake and becomes a `ValueError`; anything else
/// is the server's problem and becomes a `RuntimeError`.
impl From<SearchError> for PyErr {
    fn from(error: SearchError) -> Self {
        let message = error.to_string();
        match error {
            SearchError::QueryParse { .. } => PyValueError::new_err(message),
            _ => PyRuntimeError::new_err(message),
        }
    }
}

/// Module initialiser.
#[pymodule]
fn search_core(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(version, module)?)?;
    module.add_function(wrap_pyfunction!(open_index, module)?)?;
    module.add_function(wrap_pyfunction!(is_open, module)?)?;
    module.add_function(wrap_pyfunction!(search, module)?)?;
    module.add_function(wrap_pyfunction!(index_stats, module)?)?;
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
