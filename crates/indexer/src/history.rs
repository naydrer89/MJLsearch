//! Index history: how much was indexed, and when.
//!
//! The crawl's progress file says what the *crawler* is doing; the index's own
//! growth is a different question, and it is the one "how much arrived in the last
//! hour" needs answered. Tantivy knows how many documents exist, not when they
//! arrived, so the indexer keeps the timeline itself.
//!
//! ## Why a file and not a counter in the index
//!
//! Everything else in this project that a second process needs to read is a small
//! file written through a temporary path, because the API holds the index read-only
//! and the indexer holds the only writer. A timeline is no different, and it stays
//! out of the index schema — where a commit-time counter would have to be a field
//! on every document to serve a panel on another page.
//!
//! ## Why per-minute buckets
//!
//! A running total cannot answer "in the last hour"; it can only be differenced,
//! and differencing needs the value an hour ago. Buckets are that difference kept
//! explicitly, bounded at [`RETENTION`] entries, so the file's size is a constant
//! no matter how long the indexer runs.
//!
//! ## What a bucket counts
//!
//! What the index gained, not what the writer wrote. Re-indexing a page that is already
//! there is real work and no growth, and a "documents in the last hour" figure that
//! counted the work would report more new documents than the index holds -- which is
//! exactly the inconsistency a reader notices first. The caller therefore records the
//! difference in the document count across the commit, and the buckets sum to the same
//! thing the total reports.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Buckets kept. Three hours at one per minute, which is more than the hour the
/// panels ask about and small enough to be a rounding error on disk.
pub const RETENTION: usize = 180;

/// Seconds in the bucket width.
pub const BUCKET_SECONDS: i64 = 60;

/// Documents that became searchable during one minute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bucket {
    /// Start of the minute, as a unix timestamp in seconds.
    pub at: i64,
    /// Net documents added to the index in it.
    ///
    /// Net rather than written, and the difference is not academic: a re-crawl writes a
    /// document and adds nothing, so counting writes would publish an hour in which more
    /// became searchable than the index holds. Summing the buckets stays consistent with
    /// `total_documents` as long as they describe the same index.
    pub documents: u64,
}

/// The index's growth timeline.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexHistory {
    /// When the file was last written.
    #[serde(default)]
    pub updated_at: i64,
    /// Documents in the index after the last commit.
    #[serde(default)]
    pub total_documents: u64,
    /// Per-minute counts, oldest first.
    #[serde(default)]
    pub buckets: Vec<Bucket>,
}

impl IndexHistory {
    /// Documents within the `seconds` window ending at `now`.
    ///
    /// A partial window counts: an indexer that started eight minutes ago should
    /// report its eight minutes of work, not zero because an hour has not passed.
    pub fn documents_since(&self, now: i64, seconds: i64) -> u64 {
        let cutoff = now - seconds;
        self.buckets
            .iter()
            .filter(|bucket| bucket.at >= cutoff)
            .map(|bucket| bucket.documents)
            .sum()
    }

    /// Adds net `documents` to the bucket containing `now`.
    ///
    /// Split out from the file access so the arithmetic can be tested without a
    /// filesystem, and so the caller can see that the merge is by bucket, not by
    /// append: two commits in the same minute belong in one entry.
    pub fn add(&mut self, now: i64, documents: u64, total_documents: u64) {
        self.updated_at = now;
        self.total_documents = total_documents;
        let at = now - now.rem_euclid(BUCKET_SECONDS);

        match self.buckets.last_mut() {
            Some(last) if last.at == at => last.documents += documents,
            _ => self.buckets.push(Bucket { at, documents }),
        }

        // Keeps the file bounded, which is the whole reason buckets exist rather
        // than a log.
        if self.buckets.len() > RETENTION {
            let excess = self.buckets.len() - RETENTION;
            self.buckets.drain(..excess);
        }
    }
}

/// Reads the timeline. A missing or unreadable file is an empty history rather
/// than an error: the first run has no history, and the panels would rather show
/// a growing index than a 500.
pub fn load(path: &Path) -> IndexHistory {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return IndexHistory::default();
    };
    match serde_json::from_str::<IndexHistory>(&raw) {
        Ok(history) => history,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "discarding an unreadable index history and starting a new one"
            );
            IndexHistory::default()
        }
    }
}

/// Records a commit, atomically so a reader never sees half a timeline.
pub fn record(
    path: &Path,
    now: i64,
    documents: u64,
    total_documents: u64,
) -> std::io::Result<IndexHistory> {
    let mut history = load(path);
    history.add(now, documents, total_documents);

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let json = serde_json::to_string(&history).unwrap_or_else(|_| "{}".to_string());
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json)?;
    std::fs::rename(&temporary, path)?;

    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commits_in_the_same_minute_land_in_one_bucket() {
        let mut history = IndexHistory::default();
        // 960 and 1000 are in the same minute; 1020 is the next one.
        history.add(960, 10, 10);
        history.add(1_000, 5, 15);

        assert_eq!(history.buckets.len(), 1);
        assert_eq!(history.buckets[0].documents, 15);
        assert_eq!(history.total_documents, 15);
    }

    #[test]
    fn a_new_minute_starts_a_new_bucket() {
        let mut history = IndexHistory::default();
        history.add(1_000, 10, 10);
        history.add(1_020, 4, 14);

        assert_eq!(history.buckets.len(), 2);
        assert_eq!(history.buckets[0].at, 960);
        assert_eq!(history.buckets[1].at, 1_020);
        assert_eq!(history.buckets[1].documents, 4);
    }

    #[test]
    fn the_window_counts_only_what_is_inside_it() {
        let mut history = IndexHistory::default();
        // Three minutes of work, the first of it outside a two-minute window.
        history.add(0, 100, 100);
        history.add(120, 7, 107);
        history.add(180, 3, 110);

        assert_eq!(history.documents_since(180, 120), 10);
        assert_eq!(history.documents_since(180, 3_600), 110);
        // A window narrower than the bucket width still counts the bucket that is
        // open, because that is where the work just happened.
        assert_eq!(history.documents_since(180, 0), 3);
        // ...and one that ends before any of it counts nothing.
        assert_eq!(history.documents_since(1_000, 10), 0);
    }

    #[test]
    fn a_partial_window_is_counted_rather_than_treated_as_nothing() {
        // The indexer having started eight minutes ago must report eight minutes
        // of work; returning zero because an hour has not elapsed would make a
        // healthy run look stalled on every panel.
        let mut history = IndexHistory::default();
        history.add(0, 500, 500);

        assert_eq!(history.documents_since(480, 3_600), 500);
    }

    #[test]
    fn the_history_is_bounded_no_matter_how_long_the_indexer_runs() {
        let mut history = IndexHistory::default();
        for minute in 0..(RETENTION as i64 * 3) {
            history.add(minute * BUCKET_SECONDS, 1, minute as u64 + 1);
        }

        assert_eq!(history.buckets.len(), RETENTION);
        // Trimmed from the front: the newest minute is always present.
        let last = history.buckets.last().expect("a bucket");
        assert_eq!(last.at, (RETENTION as i64 * 3 - 1) * BUCKET_SECONDS);
        assert_eq!(history.total_documents, RETENTION as u64 * 3);
    }

    #[test]
    fn recording_round_trips_through_the_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("nested").join("history.json");

        record(&path, 600, 12, 12).expect("record");
        let history = record(&path, 660, 3, 15).expect("record again");

        assert_eq!(history.total_documents, 15);
        assert_eq!(load(&path), history);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn a_missing_or_corrupt_file_is_an_empty_history() {
        let directory = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            load(&directory.path().join("nope.json")),
            IndexHistory::default()
        );

        let corrupt = directory.path().join("broken.json");
        std::fs::write(&corrupt, "not json at all").expect("write");
        assert_eq!(load(&corrupt), IndexHistory::default());

        // ...and recording over it starts a fresh timeline rather than failing.
        let history = record(&corrupt, 60, 1, 1).expect("record");
        assert_eq!(history.buckets.len(), 1);
    }

    #[test]
    fn a_timestamp_before_the_epoch_still_buckets_sanely() {
        // `%` on a negative number is negative in Rust, so the wrong operator here
        // would bucket every pre-1970 timestamp one minute off.
        let mut history = IndexHistory::default();
        history.add(-61, 1, 1);
        assert_eq!(history.buckets[0].at, -120);
    }
}
