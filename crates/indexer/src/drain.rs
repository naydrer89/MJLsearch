//! The drain loop: read sealed segments, score them, write them, commit, retire.
//!
//! ## Why there are two passes
//!
//! A document's authority is a fact about the whole link graph, so it cannot be
//! computed from a single pass that writes as it reads. The loop therefore reads
//! every ready segment once to build the graph, computes the rank, and reads them
//! again to write. Both passes are streaming, so peak memory is the graph rather
//! than the corpus, and the second pass costs one extra decompression of a few
//! megabytes — cheaper than the alternative, which is writing every document and
//! then upserting all of them again to attach the score.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use search_core::{DocScores, DocWriter, IndexWriterConfig};

use crate::IndexerError;
use crate::graph::{self, GraphLimits, LinkGraph};
use crate::history;
use crate::spool::{for_each_document, retire, sealed_segments};

/// Configuration for a drain run.
#[derive(Debug, Clone)]
pub struct DrainConfig {
    /// Directory the crawler seals segments into.
    pub spool_dir: PathBuf,
    /// Directory of the Tantivy index to write.
    pub index_dir: PathBuf,
    /// Tantivy writer heap budget.
    pub writer_heap_bytes: usize,
    /// Commit every this many documents within a segment.
    ///
    /// The segment boundary is the real unit of work, so this only matters for
    /// unusually large segments; it exists so that one oversized segment cannot
    /// grow the uncommitted batch without limit.
    pub commit_docs: usize,
    /// Drain what is on disk and exit, instead of following the spool.
    pub once: bool,
    /// Idle sleep between polls when the spool is empty.
    pub poll_interval: Duration,
    /// Move processed segments here instead of deleting them.
    ///
    /// Useful when diagnosing a bad parse: the raw documents are the evidence.
    pub archive_dir: Option<PathBuf>,
    /// Where the growth timeline is written for the panels to read.
    pub history_path: PathBuf,
    /// Where the cross-site inlink counts are kept between runs.
    ///
    /// Persisted because host authority is a statement about the web, not about
    /// one drain: a site that other sites link to is authoritative whether or not
    /// those linking pages happened to be in today's batch.
    pub host_authority_path: PathBuf,
    /// Bounds on the in-memory link graph.
    pub graph_limits: GraphLimits,
}

impl Default for DrainConfig {
    fn default() -> Self {
        Self {
            spool_dir: PathBuf::from("data/spool"),
            index_dir: PathBuf::from("data/index/shard-000"),
            writer_heap_bytes: 64 * 1024 * 1024,
            commit_docs: 2_000,
            once: false,
            poll_interval: Duration::from_millis(500),
            archive_dir: None,
            history_path: PathBuf::from("data/index-history.json"),
            host_authority_path: PathBuf::from("data/host-authority.json"),
            graph_limits: GraphLimits::default(),
        }
    }
}

/// What a drain run did.
#[derive(Debug, Clone, Default)]
pub struct DrainReport {
    /// Segments processed.
    pub segments: u64,
    /// Documents written to the index.
    pub documents: u64,
    /// Documents in the index once the run finished.
    pub doc_count: u64,
}

/// Runs the drain loop until interrupted, or until the spool empties under
/// [`DrainConfig::once`].
///
/// ## The ordering that must not be relaxed
///
/// A segment is retired only after its documents have been committed. Retiring
/// first would mean that a crash in between loses pages that were archived and
/// parsed but never indexed, and nothing downstream could detect the loss. The
/// reverse order costs nothing on the happy path and turns a crash into a
/// replay.
///
/// A failed write deliberately does **not** advance the loop: the segment stays
/// put and the run surfaces the error, so a bad document is retried and
/// eventually noticed rather than silently skipped.
pub fn run(config: &DrainConfig) -> Result<DrainReport, IndexerError> {
    let mut writer = DocWriter::open(
        &config.index_dir,
        IndexWriterConfig {
            commit_docs: config.commit_docs,
            writer_heap_bytes: config.writer_heap_bytes,
            ..IndexWriterConfig::default()
        },
    )?;

    let mut report = DrainReport::default();
    tracing::info!(
        index_dir = %config.index_dir.display(),
        spool_dir = %config.spool_dir.display(),
        doc_count = writer.doc_count()?,
        "indexer ready"
    );

    loop {
        let ready = sealed_segments(&config.spool_dir)?;
        if ready.is_empty() {
            if config.once {
                break;
            }
            std::thread::sleep(config.poll_interval);
            continue;
        }

        let scores = build_authority(config, &ready)?;

        for path in ready {
            // Read before anything this segment writes, because the growth timeline
            // records what the index *gained*, not how much work was done: a re-crawl
            // of a page that is already indexed writes a document and gains none, and a
            // timeline that counted the write would report an hour in which more became
            // searchable than exists. See `history::record`.
            let documents_before = writer.doc_count()?;

            // Streamed, so peak memory stays at one document plus the writer's
            // own batch buffer regardless of segment size.
            let mut written = 0u64;
            let read = for_each_document(&path, |document| {
                // A document missing from the graph is written with zero authority
                // rather than skipped: it was crawled and parsed, and dropping it
                // because the graph did not mention it would lose a page to a
                // scoring detail.
                let document_scores = scores.get(&document.url).copied().unwrap_or_default();
                writer.add_or_update(&document, document_scores)?;
                written += 1;
                if writer.pending() >= config.commit_docs {
                    writer.commit()?;
                }
                Ok(())
            })?;

            writer.commit()?;
            let doc_count = writer.doc_count()?;
            // Only now is the work durable; see the ordering note above.
            retire(&path, config.archive_dir.as_deref())?;

            report.segments += 1;
            report.documents += read as u64;
            tracing::info!(
                segment = %path.display(),
                documents = read,
                committed = written,
                doc_count,
                "segment indexed"
            );

            // Written after the commit, so a timeline entry never describes work
            // that is not yet durable. A failure here is not fatal: the panels
            // lose a data point, the index does not lose a document.
            let gained = doc_count.saturating_sub(documents_before);
            match history::record(&config.history_path, now_seconds(), gained, doc_count) {
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "could not write the index history");
                }
            }
        }
    }

    report.doc_count = writer.doc_count()?;
    tracing::info!(
        segments = report.segments,
        documents = report.documents,
        doc_count = report.doc_count,
        "drain finished"
    );

    Ok(report)
}

/// Builds the link graph for everything about to be written, and persists the
/// cross-site counts it learned.
///
/// Separate from the loop so the two passes are visible as two passes rather than
/// interleaved with the write path, which is the shape that would make someone
/// reasonably ask where a document's score comes from.
fn build_authority(
    config: &DrainConfig,
    ready: &[PathBuf],
) -> Result<std::collections::HashMap<String, DocScores>, IndexerError> {
    let mut graph = LinkGraph::new(
        config.graph_limits,
        graph::load_host_inlinks(&config.host_authority_path),
    );

    for path in ready {
        for_each_document(path, |document| {
            graph.record(&document.url, &document.outlinks);
            Ok(())
        })?;
    }

    let (nodes, edges) = graph.shape();
    if graph.truncated() {
        // Reported rather than hidden: an authority score computed from part of a
        // corpus is still useful, but a reader of the log deserves to know that
        // this is what it is.
        tracing::warn!(
            max_nodes = config.graph_limits.max_nodes,
            max_edges = config.graph_limits.max_edges,
            "link graph hit its bounds; authority is relative to a partial graph"
        );
    }

    let scores = graph.finish();
    tracing::info!(
        nodes,
        edges,
        scored = scores.len(),
        hosts = graph.host_inlinks().len(),
        "link graph built"
    );

    if let Err(error) = graph::save_host_inlinks(&config.host_authority_path, graph.host_inlinks())
    {
        // Worth continuing without: the counts can be rebuilt by crawling, and
        // refusing to index because a sidecar could not be written would trade a
        // degraded result for no result.
        tracing::warn!(error = %error, "could not persist host authority");
    }

    Ok(scores)
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::Document;
    use common::spool::{open_segment_name, sealed_segment_name, write_frame};
    use std::fs::File;
    use std::io::Write;

    fn write_segment(directory: &std::path::Path, sequence: u64, documents: &[(&str, &str, &str)]) {
        let path = directory.join(sealed_segment_name(sequence));
        let mut file = File::create(&path).expect("create");
        for (url, title, body) in documents {
            let document = Document::new(*url, *title, *body, 1_700_000_000, Vec::new());
            write_frame(&mut file, &document).expect("frame");
        }
        file.flush().expect("flush");
    }

    fn config(directory: &std::path::Path) -> DrainConfig {
        DrainConfig {
            spool_dir: directory.join("spool"),
            index_dir: directory.join("index"),
            once: true,
            history_path: directory.join("history.json"),
            host_authority_path: directory.join("host-authority.json"),
            ..DrainConfig::default()
        }
    }

    #[test]
    fn a_single_pass_makes_spooled_documents_searchable() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        std::fs::create_dir_all(&config.spool_dir).expect("spool dir");

        write_segment(
            &config.spool_dir,
            1,
            &[
                (
                    "https://rust.example/a",
                    "Ownership",
                    "ownership and borrowing in rust",
                ),
                // Deliberately shares no term with the query below, so the
                // assertion on the hit list is unambiguous rather than a
                // tie-break that could flip on a scoring detail.
                ("https://rust.example/b", "Lifetimes", "dangling references"),
            ],
        );

        let report = run(&config).expect("run");

        assert_eq!(report.segments, 1);
        assert_eq!(report.documents, 2);

        // The end that matters: the documents are actually queryable.
        let engine = search_core::SearchEngine::open(&config.index_dir).expect("open index");
        assert_eq!(engine.doc_count().expect("count"), 2);
        let outcome = engine.search("borrowing", 10, 0).expect("search");
        assert_eq!(outcome.total_matches, 1);
        assert_eq!(outcome.hits[0].url, "https://rust.example/a");
    }

    #[test]
    fn a_processed_segment_is_retired_and_an_unsealed_one_is_left_alone() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        std::fs::create_dir_all(&config.spool_dir).expect("spool dir");

        write_segment(&config.spool_dir, 1, &[("https://example.com/a", "t", "b")]);

        // Still being appended to by the crawler.
        let open = config.spool_dir.join(open_segment_name(2));
        let mut file = File::create(&open).expect("create");
        write_frame(
            &mut file,
            &Document::new("https://example.com/b", "t", "b", 0, Vec::new()),
        )
        .expect("frame");
        file.flush().expect("flush");

        let report = run(&config).expect("run");

        assert_eq!(report.segments, 1);
        assert!(!config.spool_dir.join(sealed_segment_name(1)).exists());
        // Retiring the in-progress segment would have thrown away a document the
        // crawler still intends to complete.
        assert!(open.exists());
    }

    #[test]
    fn an_empty_spool_finishes_immediately_when_asked_to_once() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());

        let report = run(&config).expect("run");

        assert_eq!(report.segments, 0);
        assert_eq!(report.documents, 0);
    }

    #[test]
    fn running_twice_does_not_duplicate_documents() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        std::fs::create_dir_all(&config.spool_dir).expect("spool dir");

        write_segment(
            &config.spool_dir,
            1,
            &[("https://example.com/a", "T", "body text here")],
        );
        run(&config).expect("first run");

        // A second pass with the same URL must replace, not append. This is the
        // upsert behaviour of the `url` field doing its job end to end.
        write_segment(
            &config.spool_dir,
            2,
            &[("https://example.com/a", "T", "body text here")],
        );
        run(&config).expect("second run");

        let engine = search_core::SearchEngine::open(&config.index_dir).expect("open index");
        assert_eq!(engine.doc_count().expect("count"), 1);
    }

    #[test]
    fn processed_segments_can_be_archived_instead_of_deleted() {
        let directory = tempfile::tempdir().expect("tempdir");
        let archive = directory.path().join("done");
        let config = DrainConfig {
            archive_dir: Some(archive.clone()),
            ..config(directory.path())
        };
        std::fs::create_dir_all(&config.spool_dir).expect("spool dir");
        write_segment(&config.spool_dir, 1, &[("https://example.com/a", "t", "b")]);

        run(&config).expect("run");

        assert_eq!(sealed_segments(&archive).expect("list").len(), 1);
        assert!(sealed_segments(&config.spool_dir).expect("list").is_empty());
    }

    #[test]
    fn defaults_keep_a_bounded_batch_and_writer_heap() {
        let config = DrainConfig::default();
        assert!(config.commit_docs > 0);
        assert!(config.writer_heap_bytes <= 128 * 1024 * 1024);
        assert!(!config.once);
        assert_eq!(config.index_dir, PathBuf::from("data/index/shard-000"));
    }

    /// Writes a segment whose documents link to each other, so the graph has
    /// something to rank.
    fn write_linked_segment(directory: &std::path::Path, sequence: u64) {
        let path = directory.join(sealed_segment_name(sequence));
        let mut file = File::create(&path).expect("create");

        let home = "https://site.example/";
        let policy = "https://site.example/legal/privacy";
        let about = "https://site.example/about";

        // Both pages link home; nothing links the policy.
        let documents = [
            Document::new(
                home,
                "Site",
                "the front page of site",
                1_700_000_000,
                vec![about.to_string(), policy.to_string()],
            ),
            Document::new(
                about,
                "About Site",
                "about site and its history",
                1_700_000_000,
                vec![home.to_string()],
            ),
            Document::new(
                policy,
                "Privacy policy",
                "site privacy policy covering site data",
                1_700_000_000,
                vec![],
            ),
        ];

        for document in documents {
            write_frame(&mut file, &document).expect("frame");
        }
        file.flush().expect("flush");
    }

    #[test]
    fn a_drain_scores_documents_from_the_link_graph_it_builds() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        std::fs::create_dir_all(&config.spool_dir).expect("spool dir");
        write_linked_segment(&config.spool_dir, 1);

        run(&config).expect("run");

        let engine = search_core::SearchEngine::open(&config.index_dir).expect("open index");
        // Query for a term every page carries, so the ordering is decided by
        // authority rather than by relevance.
        let outcome = engine.search("site", 10, 0).expect("search");

        let home = outcome
            .hits
            .iter()
            .find(|hit| hit.url == "https://site.example/")
            .expect("the home page must be indexed");
        let policy = outcome
            .hits
            .iter()
            .find(|hit| hit.url == "https://site.example/legal/privacy")
            .expect("the policy must be indexed");

        assert!(
            home.authority > policy.authority,
            "home {} should outrank the unlinked policy {}",
            home.authority,
            policy.authority
        );
    }

    #[test]
    fn a_drain_reports_what_it_indexed_and_records_it_in_the_history() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        std::fs::create_dir_all(&config.spool_dir).expect("spool dir");
        write_segment(
            &config.spool_dir,
            1,
            &[
                ("https://example.com/a", "a", "body"),
                ("https://example.com/b", "b", "body"),
            ],
        );

        let report = run(&config).expect("run");

        assert_eq!(report.documents, 2);
        assert_eq!(report.doc_count, 2);

        // The panels read this file, so a run that indexes without a timeline
        // entry would show a stalled index on a healthy system.
        let history = crate::history::load(&config.history_path);
        assert_eq!(history.total_documents, 2);
        assert_eq!(history.buckets.len(), 1);
        assert_eq!(history.buckets[0].documents, 2);
    }

    #[test]
    fn re_indexing_an_existing_page_does_not_inflate_the_growth_timeline() {
        // The bug this pins: the timeline counted documents written, so replacing a page
        // that was already indexed grew the "in the last hour" figure while the index
        // stayed the same size -- and the growth page then reported more documents
        // arriving than it held, which is the first inconsistency a reader notices.
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        std::fs::create_dir_all(&config.spool_dir).expect("spool dir");

        write_segment(
            &config.spool_dir,
            1,
            &[("https://example.com/a", "T", "body text here")],
        );
        run(&config).expect("first run");

        // The same page again: real work for the writer, no growth for the index.
        write_segment(
            &config.spool_dir,
            2,
            &[("https://example.com/a", "T", "body text here")],
        );
        run(&config).expect("second run");

        let history = crate::history::load(&config.history_path);
        assert_eq!(history.total_documents, 1);
        let counted: u64 = history.buckets.iter().map(|bucket| bucket.documents).sum();
        assert_eq!(
            counted, 1,
            "the timeline must agree with the document count"
        );
    }

    #[test]
    fn host_authority_persists_between_runs() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        std::fs::create_dir_all(&config.spool_dir).expect("spool dir");

        // A page on one host linking to another.
        let path = config.spool_dir.join(sealed_segment_name(1));
        let mut file = File::create(&path).expect("create");
        write_frame(
            &mut file,
            &Document::new(
                "https://blog.example/post",
                "A post",
                "worth reading",
                1_700_000_000,
                vec!["https://target.example/".to_string()],
            ),
        )
        .expect("frame");
        file.flush().expect("flush");

        run(&config).expect("first run");
        run(&config).expect("second run");

        let counts = graph::load_host_inlinks(&config.host_authority_path);
        assert_eq!(counts.get("target.example"), Some(&1));
    }

    #[test]
    fn a_document_the_graph_never_saw_is_still_indexed() {
        // The graph is built from the same documents, so a miss means a bug in the
        // graph -- and losing the page over it would be the wrong failure.
        let directory = tempfile::tempdir().expect("tempdir");
        let config = config(directory.path());
        std::fs::create_dir_all(&config.spool_dir).expect("spool dir");
        write_segment(&config.spool_dir, 1, &[("https://example.com/a", "t", "b")]);

        let report = run(&config).expect("run");

        assert_eq!(report.doc_count, 1);
    }
}
