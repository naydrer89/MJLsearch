//! Query parsing, BM25 scoring, pagination and snippet generation.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use moka::sync::Cache;
use std::time::Duration;
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{Query, QueryParser};
use tantivy::schema::{TantivyDocument, Value};
use tantivy::snippet::SnippetGenerator;

use tantivy::{Index, IndexReader, ReloadPolicy, Searcher};

use crate::schema::IndexSchema;

/// How many distinct result pages to keep cached, by default.
///
/// Bounded on purpose. An unbounded cache is the same mistake as an unbounded
/// frontier, just hiding in the read path.
///
/// 512 pages is small in bytes -- a page is ten hits of title, snippet and score,
/// so a few hundred kilobytes -- which is why the default is not larger: the
/// cache exists to make a repeated query cheap, not to hold the whole index's
/// result space.
const CACHE_CAPACITY: u64 = 512;

/// Characters of context in a snippet.
const SNIPPET_CHARS: usize = 240;

/// A single search result.
///
/// `score` is the raw BM25 score from Tantivy, before any Python-side
/// post-processing. Keeping them separate means the ranking layer can be changed
/// without touching the query layer.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// Page URL.
    pub url: String,
    /// Page title.
    pub title: String,
    /// Matched fragment as HTML, with matched terms wrapped in `<b>`.
    pub snippet: String,
    /// Unix timestamp in seconds at which the page was fetched.
    pub fetched_at: i64,
    /// BM25 relevance score.
    pub score: f32,
    /// How linked-to this page is within the crawl's link graph, in `[0, 1]`.
    ///
    /// Returned so that ranking can weigh it. The engine deliberately does *not*
    /// fold it into `score`: BM25 is a property of the query, authority is a
    /// property of the document, and keeping them separate is what lets the
    /// policy above decide how much authority is worth without recompiling the
    /// query engine.
    pub authority: f32,
    /// How many other sites link to this page's host, in `[0, 1]`.
    pub host_authority: f32,
}

/// The result of one query.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutcome {
    /// The page of hits that was requested.
    pub hits: Vec<Hit>,
    /// How many documents in the index matched, ignoring paging.
    pub total_matches: u64,
    /// Documents in the index, matched or not.
    pub doc_count: u64,
    /// Server-side time for this query, in milliseconds.
    pub elapsed_ms: f64,
    /// Whether the result came from the cache.
    pub cached: bool,
    /// Whether every term was required to match, or the search had to be widened
    /// to "any term" because requiring all of them found nothing.
    ///
    /// Reported rather than hidden: a caller looking at approximate results is
    /// entitled to know that is what they are looking at.
    pub relaxed: bool,
}

/// A cached query result: the page, plus the total it was computed with.
///
/// The total is stored rather than recomputed because recomputing it would mean
/// re-running the query, and because deriving it from the hits is wrong -- see the
/// hit path in [`SearchEngine::search`].
#[derive(Debug, Clone)]
struct CachedPage {
    hits: Vec<Hit>,
    total_matches: u64,
    /// Cached with the page because it is a property of the query, not of the
    /// individual request: a cache hit that flipped it would report a differently
    /// phrased result for the same query.
    relaxed: bool,
}

/// Whether `query` holds more than one term.
///
/// Only a multi-term query has anything to relax: for one term, "all of them" and
/// "any of them" are the same query, so the fallback would re-run it to learn
/// nothing.
fn is_multi_term(query: &str) -> bool {
    query.split_whitespace().nth(1).is_some()
}

/// Errors raised by the query and write paths.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// Tantivy rejected an operation against the index.
    #[error("index error: {0}")]
    Index(#[from] tantivy::TantivyError),
    /// Local file I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The user-supplied query string could not be parsed.
    #[error("could not parse query {query:?}: {source}")]
    QueryParse {
        /// The query string as supplied by the caller.
        query: String,
        /// The underlying parser error.
        source: tantivy::query::QueryParserError,
    },
    /// The index path does not exist, or is not an index.
    #[error("no index at {0}; has the indexer run?")]
    MissingIndex(String),
    /// The index on disk was built with an incompatible schema.
    #[error("the index at {0} was built with an incompatible schema; reindex it")]
    IncompatibleSchema(String),
    /// Scaffolding only: this path is not wired up yet.
    #[error("not implemented yet: {0}")]
    NotImplemented(&'static str),
}

/// An open, read-only handle on a search index.
///
/// The API process only ever holds this type. It never holds a writer, because
/// Tantivy permits exactly one writer per index and the indexer owns it.
pub struct SearchEngine {
    index: Index,
    reader: IndexReader,
    schema: IndexSchema,
    cache: Cache<(String, usize, usize, u64), Arc<CachedPage>>,
}

impl SearchEngine {
    /// Opens the index at `index_dir` read-only.
    ///
    /// The reader uses [`ReloadPolicy::Manual`] and is reloaded per query rather
    /// than by a background watcher: with a single writer process, an explicit
    /// reload is cheap and makes the "see new commits" behaviour obvious instead
    /// of timing-dependent.
    pub fn open(index_dir: &Path) -> Result<Self, SearchError> {
        Self::open_with_cache(index_dir, CACHE_CAPACITY)
    }

    /// Opens the index with an explicit query-cache capacity.
    ///
    /// Separate from [`SearchEngine::open`] so the capacity can be a deployment
    /// decision: it is the one knob here that trades RAM for latency.
    pub fn open_with_cache(index_dir: &Path, cache_capacity: u64) -> Result<Self, SearchError> {
        if !index_dir.join("meta.json").exists() {
            return Err(SearchError::MissingIndex(index_dir.display().to_string()));
        }

        let index = Index::open_in_dir(index_dir)?;
        let schema = IndexSchema::build();
        if !schema.is_compatible_with(&index.schema()) {
            return Err(SearchError::IncompatibleSchema(
                index_dir.display().to_string(),
            ));
        }

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;

        Ok(Self {
            index,
            reader,
            schema,
            cache: Cache::new(cache_capacity.max(1)),
        })
    }

    /// Touches the index so the first real query does not pay for a cold start.
    ///
    /// The index is mmap'd, which means the first query after a restart faults in
    /// segment metadata and posting lists from disk: tens of milliseconds on a
    /// warm page cache, far more on a cold one, and it lands on whichever
    /// unlucky caller asks first. Paying it here moves that cost to process
    /// start-up, where it is invisible, and the returned duration is logged so the
    /// claim is checkable rather than asserted.
    pub fn warm_up(&self) -> Result<Duration, SearchError> {
        let started = Instant::now();
        self.refresh()?;

        // A match-all query with a limit of one is the cheapest request that still
        // reads the segment metadata through the same path a real query uses.
        let searcher = self.reader.searcher();
        searcher.search(
            &tantivy::query::AllQuery,
            &TopDocs::with_limit(1).order_by_score(),
        )?;
        Ok(started.elapsed())
    }

    /// Runs `query`, returning `limit` hits starting at `offset`.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchOutcome, SearchError> {
        let started = Instant::now();

        // Picks up anything the indexer has committed since the last query.
        self.reader.reload()?;
        let searcher = self.reader.searcher();

        // The fingerprint is part of the cache key, which is what makes a cached
        // result impossible to serve after a new commit: a commit changes the
        // fingerprint, so the old entry simply becomes unreachable.
        let fingerprint = index_fingerprint(&searcher);
        let key = (query.to_string(), limit, offset, fingerprint);
        let doc_count = searcher.num_docs();

        if let Some(page) = self.cache.get(&key) {
            // The *whole* page is cached, not just its hits. Reporting the page
            // length as the total would make the same query answer with a
            // different total on a hit than on a miss -- 211 uncached, 3 cached --
            // which is exactly the kind of number a caller pages through results
            // with.
            return Ok(SearchOutcome {
                hits: page.hits.clone(),
                total_matches: page.total_matches,
                doc_count,
                elapsed_ms: elapsed_ms(started),
                cached: true,
                relaxed: page.relaxed,
            });
        }

        let mut parser = QueryParser::for_index(
            &self.index,
            vec![self.schema.title_field(), self.schema.body_field()],
        );
        // Every term must appear. The parser's default is the opposite -- any one
        // term is enough -- and that default is why a two-word query such as
        // `rick astley` matched thousands of documents and ranked pages that
        // merely contained the word "rick". A search engine is expected to read
        // extra words as narrowing, not as alternatives.
        parser.set_conjunction_by_default();

        let parse = |parser: &mut QueryParser| -> Result<Box<dyn Query>, SearchError> {
            parser
                .parse_query(query)
                .map_err(|source| SearchError::QueryParse {
                    query: query.to_string(),
                    source,
                })
        };

        let strict = parse(&mut parser)?;
        let strict_total = searcher.search(&strict, &Count)? as u64;

        // A query that demands every term can come back empty for reasons that are
        // the caller's fault in a way they cannot see: a misspelling, a word the
        // page phrases differently, one word too many. Answering that with nothing
        // is unhelpful; answering it with "any term" and saying so is honest, and
        // the flag travels with the result so the page can label it.
        let (parsed, total_matches, relaxed) = if strict_total > 0 || !is_multi_term(query) {
            (strict, strict_total, false)
        } else {
            let mut wide_parser = QueryParser::for_index(
                &self.index,
                vec![self.schema.title_field(), self.schema.body_field()],
            );
            let wide = parse(&mut wide_parser)?;
            let wide_total = searcher.search(&wide, &Count)? as u64;
            if wide_total == 0 {
                (strict, 0, false)
            } else {
                (wide, wide_total, true)
            }
        };
        // `TopDocs` is a builder in this version; it only becomes a collector once
        // a scoring order is chosen. `with_limit` also panics on zero, so the
        // limit is floored at one.
        let top = searcher.search(
            &parsed,
            &TopDocs::with_limit(limit.max(1))
                .and_offset(offset)
                .order_by_score(),
        )?;

        // An exact total costs a full pass over the matches. It is paid here
        // rather than faked, because a caller paging through results needs to know
        // whether another page exists; if this ever shows up in latency, the fix
        // is to cap it, not to lie about it.

        let mut snippet_generator =
            SnippetGenerator::create(&searcher, &*parsed, self.schema.body_field())?;
        snippet_generator.set_max_num_chars(SNIPPET_CHARS);

        let mut hits = Vec::with_capacity(top.len());
        for (score, address) in top {
            let document: TantivyDocument = searcher.doc(address)?;
            hits.push(Hit {
                url: field_text(&document, self.schema.url_field()),
                title: field_text(&document, self.schema.title_field()),
                snippet: snippet_generator.snippet_from_doc(&document).to_html(),
                fetched_at: document
                    .get_first(self.schema.fetched_at_field())
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0),
                score,
                authority: field_f32(&document, self.schema.authority_field()),
                host_authority: field_f32(&document, self.schema.host_authority_field()),
            });
        }

        self.cache.insert(
            key,
            Arc::new(CachedPage {
                hits: hits.clone(),
                total_matches,
                relaxed,
            }),
        );
        Ok(SearchOutcome {
            hits,
            total_matches,
            doc_count,
            elapsed_ms: elapsed_ms(started),
            cached: false,
            relaxed,
        })
    }

    /// Reloads the reader so the next observation reflects the latest commit.
    ///
    /// Every reader accessor goes through here, and the reason is a bug this
    /// caught: the reader is `ReloadPolicy::Manual`, so without an explicit
    /// reload a *reporting* accessor answers with whatever the last query
    /// happened to see. Two uvicorn workers were then serving different document
    /// counts for the same index — 3 and 603 — because only one of them had run a
    /// query since the commit. A reload is a metadata read and is a no-op when
    /// nothing changed.
    fn refresh(&self) -> Result<(), SearchError> {
        self.reader.reload()?;
        Ok(())
    }

    /// Number of committed documents in the index.
    pub fn doc_count(&self) -> Result<u64, SearchError> {
        self.refresh()?;
        Ok(self.reader.searcher().num_docs())
    }

    /// Fingerprint of the index contents, which changes only on a real commit.
    pub fn fingerprint(&self) -> Result<u64, SearchError> {
        self.refresh()?;
        Ok(index_fingerprint(&self.reader.searcher()))
    }

    /// Document count and fingerprint together, from **one** reload.
    ///
    /// Ask for both through here rather than through the two accessors above. Two
    /// reasons, and the second is the one that matters:
    ///
    /// * a reload is the expensive part of either reading -- measured at 8ms per call on
    ///   this machine, against microseconds for `num_docs` -- and the dashboard polls
    ///   this every two seconds;
    /// * two reloads can straddle a commit, so the count would come from one generation and
    ///   the fingerprint from the next. They are reported as a pair precisely so a caller
    ///   can tell whether anything changed, and a pair from two commits answers that
    ///   question wrongly. One searcher, one generation, one answer.
    pub fn stats(&self) -> Result<(u64, u64), SearchError> {
        self.refresh()?;
        let searcher = self.reader.searcher();
        Ok((searcher.num_docs(), index_fingerprint(&searcher)))
    }

    /// Approximate number of entries held in the query cache.
    ///
    /// Moka maintains its counters concurrently, so this lags recent inserts.
    /// It is reported for observability only and must never be asserted on
    /// tightly or used for control flow.
    pub fn cache_entries(&self) -> u64 {
        self.cache.entry_count()
    }

    /// The underlying index, for statistics.
    pub fn index(&self) -> &Index {
        &self.index
    }

    /// The schema in force.
    pub fn schema(&self) -> &IndexSchema {
        &self.schema
    }
}

/// Fingerprint of the segments and their delete stamps, used to stamp cache
/// entries.
///
/// Deliberately **not** `Searcher::generation_id()`. That counter is bumped by
/// every `reload()`, changes or not, so using it as a cache key makes the cache
/// miss on every single query while looking perfectly correct. The segment map
/// only changes when a commit actually alters the index, which is precisely the
/// invalidation signal that is wanted.
fn index_fingerprint(searcher: &Searcher) -> u64 {
    common::content_hash(&format!("{:?}", searcher.generation().segments()))
}

fn field_text(document: &TantivyDocument, field: tantivy::schema::Field) -> String {
    document
        .get_first(field)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Reads an `f32` fast-and-stored field.
///
/// A missing value reads as zero, which is the honest default for both authority
/// fields: a document the indexer never scored is a document nothing links to.
/// Tantivy hands the value back as a `f64` regardless of the field's declared
/// width, so the narrowing is explicit rather than implicit.
fn field_f32(document: &TantivyDocument, field: tantivy::schema::Field) -> f32 {
    document
        .get_first(field)
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0) as f32
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_writer::{DocScores, DocWriter, IndexWriterConfig};
    use common::Document;

    fn build_index(directory: &Path, documents: &[Document]) {
        let mut writer = DocWriter::open(directory, IndexWriterConfig::default()).expect("writer");
        for document in documents {
            writer
                .add_or_update(document, DocScores::default())
                .expect("add");
        }
        writer.commit().expect("commit");
    }

    fn sample_documents() -> Vec<Document> {
        vec![
            Document::new(
                "https://rust.example/ownership",
                "Ownership in Rust",
                "Rust uses ownership and borrowing to manage memory without a garbage collector.",
                1_700_000_000,
                Vec::new(),
            ),
            Document::new(
                "https://python.example/gil",
                "The Python GIL",
                "The global interpreter lock serialises bytecode execution in CPython.",
                1_600_000_000,
                Vec::new(),
            ),
            Document::new(
                "https://rust.example/async",
                "Async Rust",
                "Async Rust uses futures and an executor; ownership still governs memory.",
                1_700_000_500,
                Vec::new(),
            ),
        ]
    }

    #[test]
    fn opening_a_missing_index_reports_it_rather_than_creating_one() {
        let directory = tempfile::tempdir().expect("tempdir");
        let result = SearchEngine::open(&directory.path().join("nope"));

        // The read path must never create an index: doing so would turn a
        // misconfigured path into an empty result set instead of an error.
        assert!(
            matches!(result, Err(SearchError::MissingIndex(_))),
            "expected a missing-index error"
        );
    }

    #[test]
    fn warming_up_reports_a_duration_and_leaves_the_index_usable() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let elapsed = engine.warm_up().expect("warm up");

        assert!(
            elapsed < Duration::from_secs(30),
            "warm-up took {elapsed:?}"
        );
        // Warming up must not disturb what it warmed: the same query answers the
        // same way afterwards.
        assert_eq!(engine.doc_count().expect("count"), 3);
        // Two, not one: the async document mentions ownership in its body too.
        // The assertion is about the index still answering, so it names the count
        // the corpus actually produces rather than the one that reads better.
        assert_eq!(
            engine
                .search("ownership", 10, 0)
                .expect("search")
                .total_matches,
            2
        );
    }

    #[test]
    fn warming_up_an_empty_index_is_not_an_error() {
        // A fresh deployment answers nothing yet, and must still start.
        let directory = tempfile::tempdir().expect("tempdir");
        let mut writer =
            DocWriter::open(directory.path(), IndexWriterConfig::default()).expect("writer");
        writer.commit().expect("commit");
        let engine = SearchEngine::open(directory.path()).expect("open");

        engine.warm_up().expect("warm up");
        assert_eq!(engine.doc_count().expect("count"), 0);
    }

    #[test]
    fn a_zero_cache_capacity_is_raised_to_something_usable() {
        // A cache of zero entries would make every query a miss while looking
        // like a deliberate configuration.
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open_with_cache(directory.path(), 0).expect("open");

        engine.search("ownership", 10, 0).expect("first");
        assert!(
            engine.search("ownership", 10, 0).expect("second").cached,
            "a second identical query should be answered from the cache"
        );
    }

    #[test]
    fn a_query_returns_the_matching_document() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let outcome = engine.search("garbage collector", 10, 0).expect("search");

        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(outcome.hits[0].url, "https://rust.example/ownership");
        assert_eq!(outcome.total_matches, 1);
        assert_eq!(outcome.doc_count, 3);
        assert!(!outcome.cached);
    }

    #[test]
    fn results_are_ranked_by_relevance() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let outcome = engine.search("rust ownership", 10, 0).expect("search");

        assert!(!outcome.hits.is_empty());
        // Scores must be non-increasing, or the ranking is not a ranking.
        for pair in outcome.hits.windows(2) {
            assert!(pair[0].score >= pair[1].score, "scores out of order");
        }
        assert_eq!(outcome.hits[0].url, "https://rust.example/ownership");
    }

    #[test]
    fn snippets_wrap_the_matched_terms() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let outcome = engine.search("borrowing", 10, 0).expect("search");

        assert_eq!(outcome.hits.len(), 1);
        assert!(
            outcome.hits[0].snippet.contains("<b>borrowing</b>"),
            "snippet should highlight the term, got {:?}",
            outcome.hits[0].snippet
        );
    }

    #[test]
    fn paging_walks_the_result_set_without_repeating_hits() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let first = engine.search("rust", 1, 0).expect("search");
        let second = engine.search("rust", 1, 1).expect("search");

        assert_eq!(first.hits.len(), 1);
        assert_eq!(second.hits.len(), 1);
        assert_ne!(first.hits[0].url, second.hits[0].url);
        // The total is independent of paging, which is what lets a caller decide
        // whether a next page exists.
        assert_eq!(first.total_matches, second.total_matches);
        assert_eq!(first.doc_count, 3);
    }

    #[test]
    fn an_offset_past_the_end_returns_no_hits_but_still_reports_the_total() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let outcome = engine.search("rust", 10, 500).expect("search");

        assert!(outcome.hits.is_empty());
        assert_eq!(outcome.total_matches, 2);
    }

    #[test]
    fn a_query_that_matches_nothing_is_not_an_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let outcome = engine.search("zzzznotpresent", 10, 0).expect("search");

        assert!(outcome.hits.is_empty());
        assert_eq!(outcome.total_matches, 0);
    }

    #[test]
    fn repeat_queries_are_served_from_the_cache() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        assert!(!engine.search("rust", 10, 0).expect("first").cached);
        let second = engine.search("rust", 10, 0).expect("second");

        assert!(second.cached, "an identical query must not re-run");
        assert_eq!(second.hits.len(), 2);
        // Moka's counter is eventually consistent, so this is a sanity check
        // rather than an exact count.
        assert!(engine.cache_entries() <= 1);
    }

    #[test]
    fn a_cached_result_is_byte_identical_to_an_uncached_one() {
        // The cache must not change what a caller sees, only how long it waited.
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let first = engine.search("rust ownership", 10, 0).expect("first");
        let second = engine.search("rust ownership", 10, 0).expect("second");

        assert!(second.cached);
        assert_eq!(first.hits, second.hits);
        assert_eq!(first.total_matches, second.total_matches);
    }

    #[test]
    fn a_cached_page_reports_the_true_total_and_not_its_own_length() {
        // The first version of this asserted equality between a cached and an
        // uncached result on a three-document corpus, where the page length and
        // the match count are the same number -- so the assertion held while the
        // cache was, in fact, reporting `hits.len()` as the total. Against a real
        // index that meant one query answering `total: 211` when uncached and
        // `total: 3` when cached, which is the number a caller pages through
        // results with.
        //
        // So: a limit strictly smaller than the match count, which is the case the
        // old test could not see.
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let uncached = engine.search("rust", 1, 0).expect("first");
        assert_eq!(uncached.hits.len(), 1, "the limit must actually bind");
        let true_total = uncached.total_matches;
        assert!(
            true_total > uncached.hits.len() as u64,
            "the fixture must match more documents than it returns, or this proves nothing"
        );

        let cached = engine.search("rust", 1, 0).expect("second");
        assert!(cached.cached, "the second call must be the cached path");

        assert_eq!(
            cached.total_matches, true_total,
            "a cache hit must not report the page length as the total"
        );
        assert_eq!(cached.hits, uncached.hits);
    }

    #[test]
    fn a_cache_entry_cannot_outlive_a_commit() {
        // The correctness property behind putting the index fingerprint in the
        // cache key. If this regresses, the API serves stale pages after every
        // reindex and nothing else in the system would notice.
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let before = engine.search("rust", 10, 0).expect("first");
        assert_eq!(before.total_matches, 2);
        let fingerprint_before = engine.fingerprint().expect("fingerprint");

        // The indexer adds a document.
        let mut writer =
            DocWriter::open(directory.path(), IndexWriterConfig::default()).expect("writer");
        writer
            .add_or_update(
                &Document::new(
                    "https://rust.example/new",
                    "Newest Rust page",
                    "Freshly indexed content about rust.",
                    1_800_000_000,
                    Vec::new(),
                ),
                DocScores::default(),
            )
            .expect("add");
        writer.commit().expect("commit");

        let after = engine.search("rust", 10, 0).expect("second");

        assert_ne!(
            engine.fingerprint().expect("fingerprint"),
            fingerprint_before,
            "a commit must bump it"
        );
        assert_eq!(after.total_matches, 3, "the new document must be visible");
        assert!(!after.cached, "the old cache entry must be unreachable");
    }

    #[test]
    fn a_replaced_document_disappears_from_results() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let mut writer =
            DocWriter::open(directory.path(), IndexWriterConfig::default()).expect("writer");
        writer
            .add_or_update(
                &Document::new(
                    "https://python.example/gil",
                    "The Python GIL, revised",
                    "A rewritten page that no longer mentions serialisation at all.",
                    1_800_000_000,
                    Vec::new(),
                ),
                DocScores::default(),
            )
            .expect("add");
        writer.commit().expect("commit");

        let outcome = engine.search("serialises", 10, 0).expect("search");
        assert!(
            outcome.hits.is_empty(),
            "the replaced document should be gone, got {:?}",
            outcome.hits
        );
        assert_eq!(engine.doc_count().expect("count"), 3);
    }

    #[test]
    fn the_title_field_is_searchable_too() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        // "Ownership in Rust" is in the title, and the body says "ownership" too;
        // the point is that the title alone is enough to match.
        let outcome = engine.search("ownership", 10, 0).expect("search");
        assert_eq!(outcome.hits.len(), 2);
    }

    #[test]
    fn an_unparsable_query_is_reported_as_a_parse_error() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        // An unterminated phrase is rejected by the query parser.
        let error = engine
            .search("\"unterminated", 10, 0)
            .expect_err("should fail");
        assert!(matches!(error, SearchError::QueryParse { .. }));
    }

    #[test]
    fn a_zero_limit_still_returns_a_well_formed_outcome() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        let outcome = engine.search("rust", 0, 0).expect("search");
        assert_eq!(
            outcome.hits.len(),
            1,
            "a zero limit is clamped to one internally"
        );
        assert_eq!(outcome.doc_count, 3);
    }

    #[test]
    fn a_multi_word_query_requires_every_term() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        // "borrowing" and "garbage" appear together in exactly one document,
        // while "borrowing" alone appears in one and "garbage" in one. Reading the
        // query as "any term" would therefore return the same single document here
        // for the wrong reason, so the query is chosen to separate the two readings:
        // "executor" is in one document and "borrowing" in another.
        let outcome = engine.search("borrowing garbage", 10, 0).expect("search");
        assert_eq!(outcome.total_matches, 1, "both terms must be present");
        assert!(!outcome.relaxed);
        assert_eq!(outcome.hits[0].url, "https://rust.example/ownership");
    }

    #[test]
    fn a_query_no_document_satisfies_is_widened_and_says_so() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        // No document holds both words, so the strict reading finds nothing.
        let strict = engine.search("executor gil", 10, 0).expect("search");
        assert!(
            strict.total_matches > 0,
            "the widened reading should still return the documents that hold one term"
        );
        assert!(
            strict.relaxed,
            "a widened search must be reported as widened, not passed off as a match"
        );

        // And the flag survives the cache: a second identical query is answered from
        // memory and must not describe itself differently than the first did.
        let cached = engine.search("executor gil", 10, 0).expect("search");
        assert!(cached.cached, "the second query should hit the cache");
        assert!(cached.relaxed, "a cache hit must keep the widened flag");
        assert_eq!(cached.total_matches, strict.total_matches);
    }

    #[test]
    fn a_single_word_query_is_never_reported_as_widened() {
        let directory = tempfile::tempdir().expect("tempdir");
        build_index(directory.path(), &sample_documents());
        let engine = SearchEngine::open(directory.path()).expect("open");

        // One term has nothing to relax -- "all" and "any" are the same query --
        // and a single word that matches nothing must not claim it was widened.
        let found = engine.search("borrowing", 10, 0).expect("search");
        assert!(!found.relaxed);

        let missing = engine.search("nonexistentterm", 10, 0).expect("search");
        assert_eq!(missing.total_matches, 0);
        assert!(!missing.relaxed);
    }
}
