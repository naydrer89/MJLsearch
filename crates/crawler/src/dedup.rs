//! Seen-URL deduplication.
//!
//! A growable bloom filter sized from the expected URL count. This is the one
//! deliberately bounded heap structure in the pipeline: for 100M URLs at a 1%
//! false-positive rate it costs roughly 114 MiB, against gigabytes for a
//! `HashSet` of the same URLs. Everything else that grows with the crawl lives
//! in RocksDB or in Tantivy's mmap'd index.
//!
//! The tradeoff is that a bloom filter can only answer "definitely new" or
//! "probably seen", so roughly one in a hundred new URLs is dropped as a
//! presumed duplicate. That is a deliberate, bounded loss: it costs a few percent
//! of the crawl, and the alternative costs an order of magnitude more memory.
//!
//! The filter is persisted, because otherwise every restart would re-crawl the
//! whole web from scratch.

use std::fs;
use std::path::{Path, PathBuf};

use growable_bloom_filter::GrowableBloom;

use crate::CrawlError;

/// Sizing parameters for the seen-URL filter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DedupConfig {
    /// Number of URLs the filter is provisioned for.
    pub expected_urls: usize,
    /// Target false-positive probability, e.g. `0.01` for one percent.
    pub false_positive_rate: f64,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            expected_urls: 100_000_000,
            false_positive_rate: 0.01,
        }
    }
}

impl DedupConfig {
    /// Bits required: `m = -n * ln(p) / (ln 2)^2`.
    pub fn bits(&self) -> u64 {
        if self.expected_urls == 0 || self.false_positive_rate <= 0.0 {
            return 0;
        }
        let n = self.expected_urls as f64;
        let ln2 = std::f64::consts::LN_2;
        (-(n) * self.false_positive_rate.ln() / (ln2 * ln2)).ceil() as u64
    }

    /// The same figure in bytes.
    pub fn bytes(&self) -> usize {
        (self.bits() / 8) as usize
    }

    /// Hash functions required: `k = (m / n) * ln 2`.
    pub fn hash_functions(&self) -> u32 {
        if self.expected_urls == 0 || self.bits() == 0 {
            return 0;
        }
        let m = self.bits() as f64;
        let n = self.expected_urls as f64;
        ((m / n) * std::f64::consts::LN_2).round().max(1.0) as u32
    }
}

/// The persisted set of URLs already seen.
pub struct SeenUrls {
    config: DedupConfig,
    filter: GrowableBloom,
    path: PathBuf,
    inserted: u64,
}

impl SeenUrls {
    /// Loads the filter from `path`, or starts an empty one.
    ///
    /// A corrupt or unreadable filter is replaced rather than fatal. The cost of
    /// that is re-crawling some URLs; the cost of refusing to start is a crawl
    /// that cannot run at all.
    pub fn open(path: &Path, config: DedupConfig) -> Result<Self, CrawlError> {
        let filter = match fs::read(path) {
            Ok(bytes) => {
                postcard::from_bytes::<GrowableBloom>(&bytes).unwrap_or_else(|_| fresh(config))
            }
            Err(_) => fresh(config),
        };

        Ok(Self {
            config,
            filter,
            path: path.to_path_buf(),
            inserted: 0,
        })
    }

    /// Records a URL and reports whether it was new.
    ///
    /// `false` means "probably seen" and the caller skips the URL. Derived from
    /// `contains` rather than from the return value of `insert`, because the two
    /// differ in a way that silently inverts the crawl if misread.
    pub fn check_and_insert(&mut self, url: &str) -> bool {
        if self.filter.contains(url) {
            return false;
        }
        self.filter.insert(url);
        self.inserted += 1;
        true
    }

    /// Persists the filter so a restart does not re-crawl from nothing.
    pub fn save(&self) -> Result<(), CrawlError> {
        let encoded = postcard::to_allocvec(&self.filter).map_err(CrawlError::CacheEncode)?;
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        // Written via a temporary file so that an interrupted write cannot leave
        // a half-serialised filter behind for the next run to load.
        let temporary = self.path.with_extension("bloom.tmp");
        fs::write(&temporary, &encoded)?;
        fs::rename(&temporary, &self.path)?;
        Ok(())
    }

    /// The sizing parameters in force.
    pub fn config(&self) -> DedupConfig {
        self.config
    }

    /// How many URLs this run has added.
    pub fn inserted(&self) -> u64 {
        self.inserted
    }

    /// Where the filter is persisted.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn fresh(config: DedupConfig) -> GrowableBloom {
    // The filter panics on a zero capacity or an out-of-range probability, and a
    // panic at startup would be a poor way to report a bad flag.
    GrowableBloom::new(
        config.false_positive_rate.clamp(1e-9, 0.5),
        config.expected_urls.max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    fn config() -> DedupConfig {
        DedupConfig {
            expected_urls: 10_000,
            false_positive_rate: 0.01,
        }
    }

    #[test]
    fn hundred_million_urls_at_one_percent_costs_about_114_mib() {
        let config = DedupConfig {
            expected_urls: 100_000_000,
            false_positive_rate: 0.01,
        };

        // m = -n ln(p) / (ln 2)^2 = 1e8 * 4.60517 / 0.480453 = 9.584e8 bits,
        // which is 114.25 MiB. Pinned so a change in the formula is noticed.
        let bytes = config.bytes();
        assert!(
            (110 * MIB..120 * MIB).contains(&bytes),
            "expected roughly 114 MiB, got {bytes} bytes"
        );

        // k = (m / n) ln 2 = 9.584 * 0.693 = 6.64, so 7 hashes.
        assert_eq!(config.hash_functions(), 7);
    }

    #[test]
    fn a_tighter_error_rate_costs_more_memory() {
        let base = DedupConfig {
            expected_urls: 1_000_000,
            false_positive_rate: 0.01,
        };
        let stricter = DedupConfig {
            expected_urls: 1_000_000,
            false_positive_rate: 0.0001,
        };
        assert!(stricter.bytes() > base.bytes());
    }

    #[test]
    fn memory_grows_linearly_with_the_url_count() {
        let one = DedupConfig {
            expected_urls: 1_000_000,
            false_positive_rate: 0.01,
        };
        let ten = DedupConfig {
            expected_urls: 10_000_000,
            false_positive_rate: 0.01,
        };
        assert!((ten.bytes() as f64 / one.bytes() as f64 - 10.0).abs() < 0.01);
    }

    #[test]
    fn degenerate_configuration_does_not_panic_or_divide_by_zero() {
        let empty = DedupConfig {
            expected_urls: 0,
            false_positive_rate: 0.01,
        };
        assert_eq!(empty.bits(), 0);
        assert_eq!(empty.bytes(), 0);
        assert_eq!(empty.hash_functions(), 0);

        let impossible = DedupConfig {
            expected_urls: 1_000,
            false_positive_rate: 0.0,
        };
        assert_eq!(impossible.bits(), 0);
        assert_eq!(impossible.hash_functions(), 0);
    }

    #[test]
    fn a_url_is_new_once_and_seen_afterwards() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut seen =
            SeenUrls::open(&directory.path().join("urls.bloom"), config()).expect("open");

        assert!(
            seen.check_and_insert("https://example.com/a"),
            "first sighting is new"
        );
        assert!(
            !seen.check_and_insert("https://example.com/a"),
            "second sighting is not"
        );
        assert!(seen.check_and_insert("https://example.com/b"));
        assert_eq!(seen.inserted(), 2);
    }

    #[test]
    fn the_filter_survives_being_saved_and_reloaded() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("urls.bloom");

        {
            let mut seen = SeenUrls::open(&path, config()).expect("open");
            assert!(seen.check_and_insert("https://example.com/a"));
            seen.save().expect("save");
        }

        // Without persistence every restart would re-crawl the entire web.
        let mut seen = SeenUrls::open(&path, config()).expect("reopen");
        assert!(!seen.check_and_insert("https://example.com/a"));
        assert!(seen.check_and_insert("https://example.com/never-seen"));
    }

    #[test]
    fn a_corrupt_filter_is_replaced_rather_than_fatal() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("urls.bloom");
        fs::write(&path, b"not a bloom filter").expect("write");

        // Refusing to start would be a worse outcome than re-crawling.
        let mut seen = SeenUrls::open(&path, config()).expect("open");
        assert!(seen.check_and_insert("https://example.com/a"));
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("urls.bloom");
        let seen = SeenUrls::open(&path, config()).expect("open");
        seen.save().expect("save");

        assert!(path.exists());
        assert!(!path.with_extension("bloom.tmp").exists());
    }
}
