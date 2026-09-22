//! The canonical document representation.

use serde::{Deserialize, Serialize};

/// One fetched page, ready for storage and indexing.
///
/// This struct is the contract between all three stages, so its serialised form
/// is a persistence format. Renaming a field, reordering it in a binary codec,
/// or changing [`content_hash`] invalidates data that is already on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    /// Absolute, normalised URL of the page.
    pub url: String,
    /// Contents of `<title>`, or a derived fallback when the page has none.
    pub title: String,
    /// Extracted main text, with navigation, headers and footers removed.
    pub body: String,
    /// Unix timestamp in seconds at which the page was fetched.
    pub fetched_at: i64,
    /// [`content_hash`] of `body`; used for exact-duplicate suppression.
    pub content_hash: u64,
    /// Absolute URLs discovered on the page, for the frontier.
    pub outlinks: Vec<String>,
}

/// Content hash of a page body.
///
/// xxh3 rather than `std::hash::DefaultHasher`: the standard library explicitly
/// does not guarantee that `DefaultHasher`'s algorithm is stable across Rust
/// releases, and this value is persisted. A changed algorithm would silently
/// orphan every hash already written to disk, so the choice of function is
/// load-bearing and is pinned by a golden-value test below.
///
/// The hash covers the exact bytes of `body`. Near-duplicate detection (shingle
/// hashing, SimHash) is a separate concern and must not be folded in here, or
/// exact-duplicate suppression would start rejecting pages it should keep.
#[inline]
pub fn content_hash(body: &str) -> u64 {
    xxhash_rust::xxh3::xxh3_64(body.as_bytes())
}

impl Document {
    /// Builds a document, deriving `content_hash` from `body` so that the two
    /// can never disagree.
    pub fn new(
        url: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
        fetched_at: i64,
        outlinks: Vec<String>,
    ) -> Self {
        let body = body.into();
        Self {
            url: url.into(),
            title: title.into(),
            content_hash: content_hash(&body),
            body,
            fetched_at,
            outlinks,
        }
    }

    /// Recomputes `content_hash` after `body` was mutated directly.
    pub fn refresh_content_hash(&mut self) {
        self.content_hash = content_hash(&self.body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pinned xxh3-64 digest of `"hello world"`.
    ///
    /// If this test fails, the content hash algorithm changed, and every
    /// `content_hash` already persisted in the queue and in the index is
    /// orphaned. Treat that as a migration, not as a constant to update.
    const GOLDEN_HASH: u64 = 15_296_390_279_056_496_779;

    fn sample() -> Document {
        Document::new(
            "https://example.com/a",
            "Example",
            "hello world",
            1_700_000_000,
            vec!["https://example.com/b".to_string()],
        )
    }

    #[test]
    fn new_derives_the_content_hash_from_the_body() {
        assert_eq!(sample().content_hash, content_hash("hello world"));
    }

    #[test]
    fn content_hash_algorithm_is_pinned() {
        assert_eq!(content_hash("hello world"), GOLDEN_HASH);
    }

    #[test]
    fn refresh_recomputes_after_a_direct_body_mutation() {
        let mut doc = sample();
        doc.body.push('!');
        assert_ne!(doc.content_hash, content_hash(&doc.body));

        doc.refresh_content_hash();
        assert_eq!(doc.content_hash, content_hash(&doc.body));
    }

    #[test]
    fn content_hash_sees_trailing_whitespace_as_a_difference() {
        assert_ne!(content_hash("hello world"), content_hash("hello world "));
    }

    #[test]
    fn serde_round_trip_preserves_every_field() {
        let doc = sample();
        let json = serde_json::to_string(&doc).expect("serialise");
        let restored: Document = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(doc, restored);
    }
}
