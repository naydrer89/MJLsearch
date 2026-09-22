//! Runs the indexer.
//!
//! Runs until interrupted. Nothing about reindexing is exposed over HTTP: the API
//! is read-only with respect to the index, so a request handler can never trigger
//! a write to it.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use indexer::DrainConfig;

/// Command-line configuration for the indexer.
#[derive(Debug, Parser)]
#[command(
    name = "indexer",
    version,
    about = "Drains spooled documents from the crawler into the Tantivy index."
)]
struct Cli {
    /// Directory the crawler seals spool segments into.
    #[arg(long, env = "SEARCH_SPOOL_DIR", default_value = "data/spool")]
    spool_dir: PathBuf,

    /// Directory of the Tantivy index to write.
    #[arg(long, env = "SEARCH_INDEX_DIR", default_value = "data/index/shard-000")]
    index_dir: PathBuf,

    /// Commit every this many documents within a segment.
    #[arg(long, default_value_t = 2_000)]
    commit_docs: usize,

    /// Tantivy writer heap budget, in MiB.
    #[arg(long, default_value_t = 64)]
    writer_heap_mib: usize,

    /// Drain what is on disk and exit, rather than following the spool.
    #[arg(long)]
    once: bool,

    /// Idle sleep between polls when the spool is empty, in milliseconds.
    #[arg(long, default_value_t = 500)]
    poll_interval_ms: u64,

    /// Move processed segments here instead of deleting them.
    ///
    /// Worth setting when a parse looks wrong: the spooled documents are the
    /// evidence of what the crawler actually extracted.
    #[arg(long, env = "SEARCH_SPOOL_DONE_DIR")]
    archive_dir: Option<PathBuf>,

    /// Where the growth timeline is written, for the panels that read it.
    #[arg(
        long,
        env = "SEARCH_INDEX_HISTORY",
        default_value = "data/index-history.json"
    )]
    history_path: PathBuf,

    /// Where cross-site inlink counts are kept between runs.
    #[arg(
        long,
        env = "SEARCH_HOST_AUTHORITY",
        default_value = "data/host-authority.json"
    )]
    host_authority_path: PathBuf,

    /// Maximum distinct URLs held in the in-memory link graph.
    #[arg(long, default_value_t = 250_000)]
    max_graph_nodes: usize,

    /// Maximum links held in the in-memory link graph.
    #[arg(long, default_value_t = 2_000_000)]
    max_graph_edges: usize,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .json()
        .init();

    let cli = Cli::parse();

    let config = DrainConfig {
        spool_dir: cli.spool_dir,
        index_dir: cli.index_dir,
        writer_heap_bytes: cli.writer_heap_mib * 1024 * 1024,
        commit_docs: cli.commit_docs,
        once: cli.once,
        poll_interval: Duration::from_millis(cli.poll_interval_ms),
        archive_dir: cli.archive_dir,
        history_path: cli.history_path,
        host_authority_path: cli.host_authority_path,
        graph_limits: indexer::GraphLimits {
            max_nodes: cli.max_graph_nodes,
            max_edges: cli.max_graph_edges,
        },
    };

    tracing::info!(
        spool_dir = %config.spool_dir.display(),
        index_dir = %config.index_dir.display(),
        commit_docs = config.commit_docs,
        writer_heap_mib = config.writer_heap_bytes / (1024 * 1024),
        once = config.once,
        history_path = %config.history_path.display(),
        host_authority_path = %config.host_authority_path.display(),
        "indexer starting"
    );

    match indexer::run(&config) {
        Ok(report) => {
            tracing::info!(
                segments = report.segments,
                documents = report.documents,
                doc_count = report.doc_count,
                "indexer stopped"
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!(error = %error, "indexer failed");
            ExitCode::FAILURE
        }
    }
}
