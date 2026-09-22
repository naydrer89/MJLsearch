//! The crawler's RocksDB handle.
//!
//! One handle, opened once, shared by the frontier and the robots cache. RocksDB
//! takes an exclusive lock on its directory, so a second handle in the same
//! process -- or a second process -- would simply fail to open. That constraint
//! is also why the document hand-off to the indexer is a spool directory of
//! files rather than another column family: see [`common::spool`].

use std::path::Path;

use rocksdb::{ColumnFamily, ColumnFamilyDescriptor, DB, IteratorMode, Options, WriteBatch};

use crate::CrawlError;

/// Column family holding URLs still to be fetched, keyed by host then sequence.
pub const CF_FRONTIER_PENDING: &str = "frontier_pending";
/// Column family holding per-host politeness state.
pub const CF_FRONTIER_HOSTS: &str = "frontier_hosts";
/// Column family holding cached robots.txt rules.
pub const CF_ROBOTS: &str = "robots";
/// Column family holding small counters.
pub const CF_META: &str = "meta";

/// Every column family this database contains.
pub const COLUMN_FAMILIES: [&str; 4] = [CF_FRONTIER_PENDING, CF_FRONTIER_HOSTS, CF_ROBOTS, CF_META];

/// Key of the frontier sequence counter in [`CF_META`].
pub const META_FRONTIER_SEQ: &[u8] = b"frontier_seq";
/// Key of the exact count of queued URLs in [`CF_META`].
pub const META_PENDING_COUNT: &[u8] = b"pending_count";
/// Key of the exact count of known hosts in [`CF_META`].
pub const META_HOST_COUNT: &[u8] = b"host_count";

/// Tuning for the embedded database.
#[derive(Debug, Clone)]
pub struct DbConfig {
    /// Block cache per column family, in MiB.
    ///
    /// This is the knob that keeps the frontier's read path from growing into
    /// RAM: RocksDB will otherwise size its caches from the machine's total
    /// memory, which on a 23 GiB host is exactly the failure mode this project
    /// exists to avoid.
    pub block_cache_mb: u64,
    /// Write buffer per column family, in MiB.
    pub write_buffer_mb: u64,
    /// Upper bound on open file descriptors held by the database.
    pub max_open_files: i32,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            block_cache_mb: 32,
            write_buffer_mb: 16,
            max_open_files: 512,
        }
    }
}

/// The crawler's database.
pub struct CrawlDb {
    db: DB,
}

impl CrawlDb {
    /// Opens, creating the directory and any missing column families.
    pub fn open(path: &Path, config: &DbConfig) -> Result<Self, CrawlError> {
        std::fs::create_dir_all(path)?;

        let mut options = Options::default();
        options.create_if_missing(true);
        options.create_missing_column_families(true);
        options.set_max_open_files(config.max_open_files);
        options.set_write_buffer_size(mib(config.write_buffer_mb));
        // The default column family is unused; give it a small cache too rather
        // than letting it inherit an unbounded one.
        options.optimize_for_point_lookup(config.block_cache_mb);

        let descriptors: Vec<ColumnFamilyDescriptor> = COLUMN_FAMILIES
            .iter()
            .map(|name| {
                let mut cf_options = Options::default();
                cf_options.set_write_buffer_size(mib(config.write_buffer_mb));
                cf_options.optimize_for_point_lookup(config.block_cache_mb);
                ColumnFamilyDescriptor::new(*name, cf_options)
            })
            .collect();

        let db = DB::open_cf_descriptors(&options, path, descriptors)?;
        Ok(Self { db })
    }

    /// The underlying handle.
    pub fn db(&self) -> &DB {
        &self.db
    }

    /// Resolves a column family by name.
    pub fn cf(&self, name: &'static str) -> Result<&ColumnFamily, CrawlError> {
        self.db
            .cf_handle(name)
            .ok_or(CrawlError::MissingColumnFamily(name))
    }

    /// Reads a big-endian `u64` counter, defaulting to zero.
    pub fn read_counter(&self, key: &[u8]) -> Result<u64, CrawlError> {
        match self.db.get_cf(self.cf(CF_META)?, key)? {
            Some(bytes) => Ok(<[u8; 8]>::try_from(bytes.as_slice())
                .map(u64::from_be_bytes)
                .unwrap_or(0)),
            None => Ok(0),
        }
    }

    /// Persists a counter together with a queued write, in one batch.
    ///
    /// Batching matters: a counter written separately from the entry it
    /// describes can, after a crash, disagree with it.
    pub fn write_counter(&self, key: &[u8], value: u64) -> Result<(), CrawlError> {
        self.db
            .put_cf(self.cf(CF_META)?, key, value.to_be_bytes())?;
        Ok(())
    }

    /// Approximate entry count for a column family.
    ///
    /// RocksDB's own estimate, which is cheap and deliberately not exact: it is
    /// used for reporting, never for correctness.
    pub fn count_estimate(&self, name: &'static str) -> Result<u64, CrawlError> {
        Ok(self
            .db
            .property_int_value_cf(self.cf(name)?, "rocksdb.estimate-num-keys")?
            .unwrap_or(0))
    }

    /// Exact entry count for a column family, by iteration.
    ///
    /// O(keys), so it is not for the hot path: it exists to *reconcile* the cheap
    /// counters at start-up, which is where an estimate is not good enough. The
    /// estimate is useless at small sizes — it reported 601 hosts for a crawl of a
    /// single host — and a dashboard number that is wrong by three orders of
    /// magnitude is worse than no number.
    pub fn count_exact(&self, name: &'static str) -> Result<u64, CrawlError> {
        let mut total = 0u64;
        for entry in self.db.iterator_cf(self.cf(name)?, IteratorMode::Start) {
            entry?;
            total += 1;
        }
        Ok(total)
    }

    /// Applies a batch of writes atomically.
    pub fn write_batch(&self, batch: WriteBatch) -> Result<(), CrawlError> {
        self.db.write(batch)?;
        Ok(())
    }
}

fn mib(value: u64) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX / (1024 * 1024)) * 1024 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_every_declared_column_family() {
        let directory = tempfile::tempdir().expect("tempdir");
        let db = CrawlDb::open(directory.path(), &DbConfig::default()).expect("open");

        for name in COLUMN_FAMILIES {
            assert!(db.cf(name).is_ok(), "column family {name} should be open");
        }
    }

    #[test]
    fn counters_default_to_zero_and_round_trip() {
        let directory = tempfile::tempdir().expect("tempdir");
        let db = CrawlDb::open(directory.path(), &DbConfig::default()).expect("open");

        assert_eq!(db.read_counter(META_FRONTIER_SEQ).expect("read"), 0);
        db.write_counter(META_FRONTIER_SEQ, 4_242).expect("write");
        assert_eq!(db.read_counter(META_FRONTIER_SEQ).expect("read"), 4_242);
    }

    #[test]
    fn a_reopened_database_keeps_its_counters() {
        let directory = tempfile::tempdir().expect("tempdir");
        {
            let db = CrawlDb::open(directory.path(), &DbConfig::default()).expect("open");
            db.write_counter(META_FRONTIER_SEQ, 99).expect("write");
        }
        let db = CrawlDb::open(directory.path(), &DbConfig::default()).expect("reopen");
        assert_eq!(db.read_counter(META_FRONTIER_SEQ).expect("read"), 99);
    }
}
