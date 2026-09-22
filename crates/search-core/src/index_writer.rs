//! Index writes.
//!
//! Tantivy permits exactly one writer process per index, enforced by a lock file.
//! This module owns that writer, and it is used by the `indexer` binary only: the
//! crawler never touches the index, and the API process opens it read-only. That
//! split is what keeps a slow commit from stalling fetching.

use std::path::Path;
use std::time::{Duration, Instant};

use common::Document;
use tantivy::schema::TantivyDocument;
use tantivy::{Index, IndexWriter, Term};

use crate::schema::IndexSchema;
use crate::search::SearchError;

/// Tantivy refuses to open a writer with a budget below this.
const MIN_HEAP_BYTES: usize = 15 * 1024 * 1024;

/// The two link-graph scores a document carries into the index.
///
/// Kept in the engine rather than derived at query time because they are
/// properties of the *whole* crawl, not of one query: computing them per request
/// would mean loading the entire link graph into the read path. Splitting them
/// into two numbers rather than one blended "quality" is deliberate — the page's
/// own authority and its site's are different claims, and the ranking policy that
/// weighs them should be able to decide how much each is worth.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DocScores {
    /// How linked-to this page is within the crawled link graph, in `[0, 1]`,
    /// normalised so that an average page sits near zero and a hub near one.
    pub authority: f32,
    /// How many *other sites* link to this page's host, in `[0, 1]`.
    pub host_authority: f32,
}

/// Commit and memory policy for the index writer.
#[derive(Debug, Clone)]
pub struct IndexWriterConfig {
    /// Commit after this many documents have been added.
    pub commit_docs: usize,
    /// ...or after this long, whichever comes first.
    pub commit_interval: Duration,
    /// Tantivy's writer heap budget.
    ///
    /// This is the one place where writing the index costs RAM, and it is bounded
    /// explicitly rather than left at a default that scales with the machine.
    /// Larger batches amortise segment creation but raise both peak memory and
    /// the amount of work lost to an unclean shutdown.
    pub writer_heap_bytes: usize,
}

impl Default for IndexWriterConfig {
    fn default() -> Self {
        Self {
            commit_docs: 1_000,
            commit_interval: Duration::from_secs(30),
            writer_heap_bytes: 64 * 1024 * 1024,
        }
    }
}

impl IndexWriterConfig {
    /// The heap budget, floored at what Tantivy will accept.
    fn effective_heap(&self) -> usize {
        self.writer_heap_bytes.max(MIN_HEAP_BYTES)
    }
}

/// The indexer's handle on the single writable Tantivy index.
pub struct DocWriter {
    index: Index,
    writer: IndexWriter,
    schema: IndexSchema,
    config: IndexWriterConfig,
    pending: usize,
    last_commit: Instant,
}

impl DocWriter {
    /// Opens the index for writing, creating it when the directory is empty.
    pub fn open(index_dir: &Path, config: IndexWriterConfig) -> Result<Self, SearchError> {
        let schema = IndexSchema::build();

        let index = if index_dir.join("meta.json").exists() {
            let index = Index::open_in_dir(index_dir)?;
            // A schema mismatch would not fail loudly on its own; it would produce
            // silently wrong results, which is far more expensive to diagnose.
            if !schema.is_compatible_with(&index.schema()) {
                return Err(SearchError::IncompatibleSchema(
                    index_dir.display().to_string(),
                ));
            }
            index
        } else {
            std::fs::create_dir_all(index_dir)?;
            Index::create_in_dir(index_dir, schema.schema().clone())?
        };

        // One writer thread: this process is the only writer, and a second thread
        // would buy little while doubling the memory held in flight.
        let writer: IndexWriter = index.writer_with_num_threads(1, config.effective_heap())?;

        Ok(Self {
            index,
            writer,
            schema,
            config,
            pending: 0,
            last_commit: Instant::now(),
        })
    }

    /// Adds a document, replacing any existing document with the same URL.
    ///
    /// The delete-then-add pair is queued on the writer and applied at the next
    /// commit, so re-indexing a URL leaves exactly one document behind rather
    /// than two. Without it, a crawl would accumulate duplicate copies of every
    /// page it visits more than once.
    pub fn add_or_update(
        &mut self,
        document: &Document,
        scores: DocScores,
    ) -> Result<(), SearchError> {
        let term = Term::from_field_text(self.schema.url_field(), &document.url);
        self.writer.delete_term(term);

        let mut row = TantivyDocument::default();
        row.add_text(self.schema.url_field(), &document.url);
        row.add_text(self.schema.title_field(), &document.title);
        row.add_text(self.schema.body_field(), &document.body);
        row.add_i64(self.schema.fetched_at_field(), document.fetched_at);
        row.add_f64(self.schema.authority_field(), f64::from(scores.authority));
        row.add_f64(
            self.schema.host_authority_field(),
            f64::from(scores.host_authority),
        );

        self.writer.add_document(row)?;
        self.pending += 1;
        Ok(())
    }

    /// Flushes everything added so far and makes it visible to readers.
    pub fn commit(&mut self) -> Result<(), SearchError> {
        self.writer.commit()?;
        self.pending = 0;
        self.last_commit = Instant::now();
        Ok(())
    }

    /// Whether the pending work should be committed now.
    pub fn should_commit(&self) -> bool {
        self.pending >= self.config.commit_docs
            || self.last_commit.elapsed() >= self.config.commit_interval
    }

    /// Documents added since the last commit.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// The commit policy in force.
    pub fn config(&self) -> &IndexWriterConfig {
        &self.config
    }

    /// The underlying index, for stats.
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// Documents visible to a fresh reader, i.e. those already committed.
    ///
    /// Deliberately here rather than at the call site: reading the count means
    /// opening a reader, which is a Tantivy operation, and keeping it in this
    /// module means the indexer binary needs no Tantivy imports of its own.
    pub fn doc_count(&self) -> Result<u64, SearchError> {
        Ok(self.index.reader()?.searcher().num_docs())
    }

    /// The schema in force.
    pub fn schema(&self) -> &IndexSchema {
        &self.schema
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::collector::Count;
    use tantivy::schema::Value;

    fn document(url: &str, title: &str, body: &str) -> Document {
        Document::new(url, title, body, 1_700_000_000, Vec::new())
    }

    fn searchable_count(writer: &DocWriter) -> u64 {
        let reader = writer.index().reader().expect("reader");
        reader.searcher().num_docs()
    }

    #[test]
    fn default_commit_policy_is_bounded_in_both_directions() {
        let config = IndexWriterConfig::default();

        // Bounded by count so a burst of slow pages cannot produce an enormous
        // uncommitted segment, and by time so a trickle of pages is still made
        // searchable promptly.
        assert!(config.commit_docs > 0);
        assert!(config.commit_interval > Duration::ZERO);
        assert!(config.writer_heap_bytes <= 128 * 1024 * 1024);
    }

    #[test]
    fn a_tiny_heap_budget_is_raised_to_what_tantivy_accepts() {
        let config = IndexWriterConfig {
            writer_heap_bytes: 1,
            ..IndexWriterConfig::default()
        };
        assert_eq!(config.effective_heap(), MIN_HEAP_BYTES);
    }

    #[test]
    fn documents_become_searchable_only_after_a_commit() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut writer =
            DocWriter::open(directory.path(), IndexWriterConfig::default()).expect("open");

        writer
            .add_or_update(
                &document("https://example.com/a", "a", "body"),
                DocScores::default(),
            )
            .expect("add");
        assert_eq!(writer.pending(), 1);
        assert_eq!(
            searchable_count(&writer),
            0,
            "nothing should be visible yet"
        );

        writer.commit().expect("commit");
        assert_eq!(writer.pending(), 0);
        assert_eq!(searchable_count(&writer), 1);
    }

    #[test]
    fn re_indexing_a_url_replaces_it_rather_than_duplicating_it() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut writer =
            DocWriter::open(directory.path(), IndexWriterConfig::default()).expect("open");

        writer
            .add_or_update(
                &document("https://example.com/a", "first", "one"),
                DocScores::default(),
            )
            .expect("add");
        writer.commit().expect("commit");

        writer
            .add_or_update(
                &document("https://example.com/a", "second", "two"),
                DocScores::default(),
            )
            .expect("add");
        writer.commit().expect("commit");

        // The whole point of indexing `url` with the raw tokenizer.
        assert_eq!(searchable_count(&writer), 1);
    }

    #[test]
    fn an_existing_index_is_reopened_rather_than_recreated() {
        let directory = tempfile::tempdir().expect("tempdir");
        {
            let mut writer =
                DocWriter::open(directory.path(), IndexWriterConfig::default()).expect("open");
            writer
                .add_or_update(
                    &document("https://example.com/a", "t", "b"),
                    DocScores::default(),
                )
                .expect("add");
            writer.commit().expect("commit");
        }

        let writer =
            DocWriter::open(directory.path(), IndexWriterConfig::default()).expect("reopen");
        assert_eq!(searchable_count(&writer), 1);
    }

    #[test]
    fn the_commit_policy_fires_on_the_document_count() {
        let directory = tempfile::tempdir().expect("tempdir");
        let config = IndexWriterConfig {
            commit_docs: 2,
            commit_interval: Duration::from_secs(3_600),
            ..IndexWriterConfig::default()
        };
        let mut writer = DocWriter::open(directory.path(), config).expect("open");

        assert!(!writer.should_commit());
        writer
            .add_or_update(
                &document("https://example.com/a", "t", "b"),
                DocScores::default(),
            )
            .expect("add");
        assert!(!writer.should_commit());
        writer
            .add_or_update(
                &document("https://example.com/b", "t", "b"),
                DocScores::default(),
            )
            .expect("add");
        assert!(writer.should_commit());
    }

    #[test]
    fn every_schema_field_round_trips_into_a_document() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut writer =
            DocWriter::open(directory.path(), IndexWriterConfig::default()).expect("open");

        let original = Document::new(
            "https://example.com/a",
            "A Title",
            "some searchable body",
            1_700_000_123,
            vec!["https://example.com/b".into()],
        );
        writer
            .add_or_update(&original, DocScores::default())
            .expect("add");
        writer.commit().expect("commit");

        let reader = writer.index().reader().expect("reader");
        let searcher = reader.searcher();
        assert_eq!(
            searcher
                .search(&tantivy::query::AllQuery, &Count)
                .expect("count"),
            1
        );

        let stored: TantivyDocument = searcher
            .doc(tantivy::DocAddress::new(0, 0))
            .expect("fetch document");
        let schema = writer.schema();
        let field_text = |field| {
            stored
                .get_first(field)
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string()
        };

        assert_eq!(field_text(schema.url_field()), "https://example.com/a");
        assert_eq!(field_text(schema.title_field()), "A Title");
        assert_eq!(field_text(schema.body_field()), "some searchable body");
        assert_eq!(
            stored
                .get_first(schema.fetched_at_field())
                .and_then(|value| value.as_i64()),
            Some(1_700_000_123)
        );
    }

    #[test]
    fn supplied_authority_scores_survive_the_round_trip() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut writer =
            DocWriter::open(directory.path(), IndexWriterConfig::default()).expect("open");

        writer
            .add_or_update(
                &document("https://example.com/a", "A", "body"),
                DocScores {
                    authority: 0.75,
                    host_authority: 0.5,
                },
            )
            .expect("add");
        writer.commit().expect("commit");

        let reader = writer.index().reader().expect("reader");
        let stored: TantivyDocument = reader
            .searcher()
            .doc(tantivy::DocAddress::new(0, 0))
            .expect("fetch document");
        let schema = writer.schema();
        let number = |field| {
            stored
                .get_first(field)
                .and_then(|value| value.as_f64())
                .expect("a numeric value") as f32
        };

        // The ranking pass reads these back; a silent zero here would make the
        // whole link-graph computation invisible at query time.
        assert!((number(schema.authority_field()) - 0.75).abs() < 1e-6);
        assert!((number(schema.host_authority_field()) - 0.5).abs() < 1e-6);
    }
}
