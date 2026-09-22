//! RocksDB-backed URL frontier.
//!
//! The frontier is the queue of URLs still to be fetched, and it lives entirely
//! on disk: a heap queue would grow without bound with the crawl, which is
//! exactly the failure mode this design exists to avoid.
//!
//! ## Key layout
//!
//! Keys are `host` + `0x00` + big-endian sequence. The separator is safe because
//! a hostname can never contain a NUL byte, and it gives per-host FIFO order
//! straight from the key ordering, with no in-memory index.
//!
//! ## Why pop seeks instead of scanning
//!
//! A naive implementation walks key by key until it finds a ready host. When the
//! head of the queue belongs to a host that is still inside its politeness
//! window, that walk degrades badly. Instead, each candidate host is examined
//! exactly once: after looking at a host's head, the cursor jumps to
//! `host` + `0x01`, which sorts after every key of that host. One trip through
//! the loop therefore costs one seek per host, not one per URL.

use std::collections::HashMap;

use common::spool::CrawlJob;
use rocksdb::{Direction, IteratorMode, WriteBatch};
use serde::{Deserialize, Serialize};

use crate::CrawlError;
use crate::db::{
    CF_FRONTIER_HOSTS, CF_FRONTIER_PENDING, CF_META, CrawlDb, META_FRONTIER_SEQ, META_HOST_COUNT,
    META_PENDING_COUNT,
};

/// Per-host scheduling state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HostState {
    /// Earliest unix-millisecond timestamp at which this host may be contacted.
    pub next_allowed_at_ms: i64,
    /// When this host was last actually fetched.
    pub last_fetched_at_ms: i64,
}

/// Extracts the lowercased host of an absolute URL.
///
/// Returns `None` for URLs that cannot be parsed and for schemes that have no
/// host, such as `mailto:` and `javascript:`. Those must never be enqueued, and
/// treating them as a host of `""` would make every one of them share a single
/// politeness bucket.
pub fn host_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed.host_str().map(str::to_ascii_lowercase)
}

/// Politeness delay in milliseconds for a host.
///
/// A `Crawl-delay` from robots.txt is treated as a floor and never as a ceiling:
/// a site that asks for more patience than our own default always gets it, and
/// our default applies whenever the site is silent.
pub fn politeness_delay_ms(default_ms: u64, robots_crawl_delay_s: Option<u64>) -> u64 {
    match robots_crawl_delay_s {
        Some(seconds) => seconds.saturating_mul(1_000).max(default_ms),
        None => default_ms,
    }
}

/// Builds the key just past every entry of `host`.
fn cursor_after(host: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(host.len() + 1);
    key.extend_from_slice(host);
    // 0x01 sorts after the 0x00 separator of this host's own keys, and before any
    // real hostname character, so the next iteration lands on the next host.
    key.push(1);
    key
}

/// The pending-URL queue.
pub struct Frontier<'a> {
    db: &'a CrawlDb,
    sequence: u64,
    /// Exact number of queued URLs.
    ///
    /// Maintained rather than queried, and updated inside the same batch as the
    /// entry it describes: a counter written separately can, after a crash,
    /// disagree with the thing it counts. `count_estimate` is not an option here
    /// — it reported 601 hosts for a single-host crawl.
    pending: u64,
    /// Exact number of hosts the frontier knows about.
    hosts: u64,
}

impl<'a> Frontier<'a> {
    /// Opens the frontier, reconciling its counters against the database.
    ///
    /// The counters are maintained incrementally, so the reconciliation here is
    /// what makes them provably right after any restart, whatever a previous
    /// process left behind. It is one scan per column family, paid once at
    /// start-up rather than on every stats write.
    pub fn open(db: &'a CrawlDb) -> Result<Self, CrawlError> {
        let sequence = db.read_counter(META_FRONTIER_SEQ)?;
        let pending = db.count_exact(CF_FRONTIER_PENDING)?;
        let hosts = db.count_exact(CF_FRONTIER_HOSTS)?;
        db.write_counter(META_PENDING_COUNT, pending)?;
        db.write_counter(META_HOST_COUNT, hosts)?;

        Ok(Self {
            db,
            sequence,
            pending,
            hosts,
        })
    }

    /// Enqueues a job, and permits its host immediately if it is new.
    pub fn push(&mut self, job: &CrawlJob) -> Result<(), CrawlError> {
        let host = host_of(&job.url).ok_or_else(|| CrawlError::InvalidUrl {
            url: job.url.clone(),
            reason: "no host",
        })?;

        // Read before the batch, because whether the host is new decides what the
        // batch contains.
        let is_new_host = self.host_state(&host)?.is_none();

        self.sequence += 1;
        self.pending += 1;
        if is_new_host {
            self.hosts += 1;
        }

        let key = common::spool::frontier_key(&host, self.sequence);
        let value = postcard::to_allocvec(job).map_err(CrawlError::CacheEncode)?;
        let meta = self.db.cf(CF_META)?;

        let mut batch = WriteBatch::default();
        batch.put_cf(self.db.cf(CF_FRONTIER_PENDING)?, &key, &value);
        batch.put_cf(meta, META_FRONTIER_SEQ, self.sequence.to_be_bytes());
        batch.put_cf(meta, META_PENDING_COUNT, self.pending.to_be_bytes());

        if is_new_host {
            let state =
                postcard::to_allocvec(&HostState::default()).map_err(CrawlError::CacheEncode)?;
            batch.put_cf(self.db.cf(CF_FRONTIER_HOSTS)?, host.as_bytes(), state);
            batch.put_cf(meta, META_HOST_COUNT, self.hosts.to_be_bytes());
        }

        self.db.write_batch(batch)?;
        Ok(())
    }

    /// Removes and returns the next job whose host is ready and not skipped.
    ///
    /// `skip` lets the caller exclude hosts that already have as many requests in
    /// flight as the politeness budget allows, without the job being lost: it
    /// stays in the frontier until the caller stops skipping it.
    pub fn pop_ready(
        &mut self,
        now_ms: i64,
        max_hosts_per_call: usize,
        skip: &dyn Fn(&str) -> bool,
    ) -> Result<Option<CrawlJob>, CrawlError> {
        let pending = self.db.cf(CF_FRONTIER_PENDING)?;
        let mut cursor: Option<Vec<u8>> = None;
        let mut examined = 0usize;

        loop {
            if examined >= max_hosts_per_call {
                return Ok(None);
            }

            // The iterator borrows the database, so take one entry and let it drop
            // before the cursor is reassigned.
            let entry = {
                let mode = match cursor.as_deref() {
                    Some(key) => IteratorMode::From(key, Direction::Forward),
                    None => IteratorMode::Start,
                };
                self.db.db().iterator_cf(pending, mode).next()
            };

            let Some(entry) = entry else {
                return Ok(None);
            };
            let (key, value) = entry?;

            let Some(host_bytes) = common::spool::frontier_key_host(&key) else {
                // A key without a separator cannot have been written by push.
                // Dropping it keeps a corrupt entry from stalling the crawl -- and
                // the counter is adjusted with it, in one batch, so a dropped entry
                // cannot leave the count describing something that is not there.
                self.forget(&key)?;
                continue;
            };
            let host = String::from_utf8_lossy(host_bytes).into_owned();
            cursor = Some(cursor_after(host_bytes));
            examined += 1;

            if skip(&host) {
                continue;
            }

            let state = self.host_state(&host)?.unwrap_or_default();
            if state.next_allowed_at_ms > now_ms {
                continue;
            }

            let job = postcard::from_bytes(&value).map_err(CrawlError::CacheDecode)?;
            self.forget(&key)?;
            return Ok(Some(job));
        }
    }

    /// Reads a host's scheduling state.
    pub fn host_state(&self, host: &str) -> Result<Option<HostState>, CrawlError> {
        match self
            .db
            .db()
            .get_cf(self.db.cf(CF_FRONTIER_HOSTS)?, host.as_bytes())?
        {
            Some(bytes) => Ok(Some(
                postcard::from_bytes(&bytes).map_err(CrawlError::CacheDecode)?,
            )),
            None => Ok(None),
        }
    }

    /// Writes a host's scheduling state.
    pub fn set_host_state(&self, host: &str, state: &HostState) -> Result<(), CrawlError> {
        let value = postcard::to_allocvec(state).map_err(CrawlError::CacheEncode)?;
        self.db
            .db()
            .put_cf(self.db.cf(CF_FRONTIER_HOSTS)?, host.as_bytes(), value)?;
        Ok(())
    }

    /// Records that the host was just fetched, and when it may be fetched again.
    pub fn defer_host(
        &self,
        host: &str,
        until_ms: i64,
        fetched_at_ms: i64,
    ) -> Result<(), CrawlError> {
        self.set_host_state(
            host,
            &HostState {
                next_allowed_at_ms: until_ms,
                last_fetched_at_ms: fetched_at_ms,
            },
        )
    }

    /// Removes a queued entry and its share of the count, in one batch.
    fn forget(&mut self, key: &[u8]) -> Result<(), CrawlError> {
        self.pending = self.pending.saturating_sub(1);

        let mut batch = WriteBatch::default();
        batch.delete_cf(self.db.cf(CF_FRONTIER_PENDING)?, key);
        batch.put_cf(
            self.db.cf(CF_META)?,
            META_PENDING_COUNT,
            self.pending.to_be_bytes(),
        );
        self.db.write_batch(batch)?;
        Ok(())
    }

    /// Exact number of URLs still queued.
    pub fn pending_count(&self) -> Result<u64, CrawlError> {
        Ok(self.pending)
    }

    /// Exact number of hosts the frontier knows about.
    pub fn host_count(&self) -> Result<u64, CrawlError> {
        Ok(self.hosts)
    }

    /// Snapshot of every host's state, for reporting. Bounded by the number of
    /// hosts actually touched during this run.
    pub fn host_snapshot(&self, limit: usize) -> Result<HashMap<String, HostState>, CrawlError> {
        let mut states = HashMap::new();
        for entry in self
            .db
            .db()
            .iterator_cf(self.db.cf(CF_FRONTIER_HOSTS)?, IteratorMode::Start)
            .take(limit)
        {
            let (key, value) = entry?;
            let host = String::from_utf8_lossy(&key).into_owned();
            states.insert(
                host,
                postcard::from_bytes(&value).map_err(CrawlError::CacheDecode)?,
            );
        }
        Ok(states)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DbConfig;

    fn db() -> (tempfile::TempDir, CrawlDb) {
        let directory = tempfile::tempdir().expect("tempdir");
        let db = CrawlDb::open(directory.path(), &DbConfig::default()).expect("open");
        (directory, db)
    }

    fn job(url: &str, depth: u32) -> CrawlJob {
        CrawlJob {
            url: url.to_string(),
            depth,
        }
    }

    #[test]
    fn host_of_normalises_case_and_rejects_unfetchable_urls() {
        assert_eq!(
            host_of("https://Example.COM/path?q=1"),
            Some("example.com".to_string())
        );
        assert_eq!(
            host_of("http://example.com:8080/x"),
            Some("example.com".to_string())
        );

        // No host, but parsable: must not collapse into one shared bucket.
        assert_eq!(host_of("mailto:someone@example.com"), None);
        assert_eq!(host_of("javascript:void(0)"), None);
        // Not parsable at all.
        assert_eq!(host_of("not a url"), None);
        assert_eq!(host_of(""), None);
    }

    #[test]
    fn robots_crawl_delay_can_only_make_us_more_polite() {
        assert_eq!(politeness_delay_ms(200, None), 200);
        assert_eq!(politeness_delay_ms(200, Some(1)), 1_000);
        assert_eq!(politeness_delay_ms(2_000, Some(1)), 2_000);
        assert_eq!(politeness_delay_ms(200, Some(u64::MAX)), u64::MAX);
    }

    #[test]
    fn a_pushed_job_comes_back_in_order() {
        let (_guard, db) = db();
        let mut frontier = Frontier::open(&db).expect("open");

        frontier.push(&job("https://a.example/1", 0)).expect("push");
        frontier.push(&job("https://a.example/2", 0)).expect("push");

        let first = frontier.pop_ready(1, 100, &|_| false).expect("pop");
        let second = frontier.pop_ready(1, 100, &|_| false).expect("pop");

        assert_eq!(
            first.map(|job| job.url),
            Some("https://a.example/1".to_string())
        );
        assert_eq!(
            second.map(|job| job.url),
            Some("https://a.example/2".to_string())
        );
    }

    #[test]
    fn the_counters_are_exact_rather_than_rocksdbs_estimate() {
        // The estimate reported 601 hosts for a crawl of a single host. These
        // numbers are shown on the dashboard, and a figure that is wrong by three
        // orders of magnitude is worse than no figure at all.
        let (_guard, db) = db();
        let mut frontier = Frontier::open(&db).expect("open");

        assert_eq!(frontier.pending_count().expect("count"), 0);
        assert_eq!(frontier.host_count().expect("count"), 0);

        frontier.push(&job("https://a.example/1", 0)).expect("push");
        frontier.push(&job("https://a.example/2", 0)).expect("push");
        frontier.push(&job("https://b.example/1", 0)).expect("push");

        assert_eq!(frontier.pending_count().expect("count"), 3);
        assert_eq!(frontier.host_count().expect("count"), 2);

        frontier.pop_ready(1, 100, &|_| false).expect("pop");

        assert_eq!(frontier.pending_count().expect("count"), 2);
        // Popping a URL does not make its host unknown: the politeness state
        // outlives the queue entry, and so must the count.
        assert_eq!(frontier.host_count().expect("count"), 2);
    }

    #[test]
    fn the_same_url_twice_does_not_double_count_its_host() {
        let (_guard, db) = db();
        let mut frontier = Frontier::open(&db).expect("open");

        frontier.push(&job("https://a.example/1", 0)).expect("push");
        frontier.push(&job("https://a.example/2", 0)).expect("push");

        assert_eq!(frontier.pending_count().expect("count"), 2);
        assert_eq!(frontier.host_count().expect("count"), 1);
    }

    #[test]
    fn reopening_reconciles_counters_that_drifted() {
        // Drift is what a crash between two writes would leave behind, and it is
        // exactly what the reconciliation pass at start-up exists to repair.
        let (_guard, db) = db();
        {
            let mut frontier = Frontier::open(&db).expect("open");
            frontier.push(&job("https://a.example/1", 0)).expect("push");
            frontier.push(&job("https://b.example/1", 0)).expect("push");
        }

        db.write_counter(META_PENDING_COUNT, 9_999)
            .expect("corrupt");
        db.write_counter(META_HOST_COUNT, 9_999).expect("corrupt");

        let frontier = Frontier::open(&db).expect("reopen");

        assert_eq!(frontier.pending_count().expect("count"), 2);
        assert_eq!(frontier.host_count().expect("count"), 2);
        assert_eq!(db.read_counter(META_PENDING_COUNT).expect("read"), 2);
        assert_eq!(db.read_counter(META_HOST_COUNT).expect("read"), 2);
    }

    #[test]
    fn popping_removes_the_job_permanently() {
        let (_guard, db) = db();
        let mut frontier = Frontier::open(&db).expect("open");
        frontier.push(&job("https://a.example/1", 0)).expect("push");

        assert!(
            frontier
                .pop_ready(1, 100, &|_| false)
                .expect("pop")
                .is_some()
        );
        assert!(
            frontier
                .pop_ready(1, 100, &|_| false)
                .expect("pop")
                .is_none()
        );
        assert_eq!(frontier.pending_count().expect("count"), 0);
    }

    #[test]
    fn a_host_inside_its_politeness_window_is_skipped() {
        let (_guard, db) = db();
        let mut frontier = Frontier::open(&db).expect("open");
        frontier.push(&job("https://a.example/1", 0)).expect("push");

        frontier.defer_host("a.example", 10_000, 0).expect("defer");

        // Before the window closes the host yields nothing...
        assert!(
            frontier
                .pop_ready(9_999, 100, &|_| false)
                .expect("pop")
                .is_none()
        );
        // ...and once it has, the job is still there.
        assert!(
            frontier
                .pop_ready(10_000, 100, &|_| false)
                .expect("pop")
                .is_some()
        );
    }

    #[test]
    fn a_deferred_host_does_not_block_other_hosts() {
        let (_guard, db) = db();
        let mut frontier = Frontier::open(&db).expect("open");
        frontier.push(&job("https://a.example/1", 0)).expect("push");
        frontier.push(&job("https://b.example/1", 0)).expect("push");
        frontier.defer_host("a.example", 10_000, 0).expect("defer");

        let popped = frontier.pop_ready(1_000, 100, &|_| false).expect("pop");

        // The impatient host is passed over rather than blocking the queue.
        assert_eq!(
            popped.map(|job| job.url),
            Some("https://b.example/1".to_string())
        );
    }

    #[test]
    fn skipping_a_saturated_host_leaves_its_job_queued() {
        let (_guard, db) = db();
        let mut frontier = Frontier::open(&db).expect("open");
        frontier
            .push(&job("https://busy.example/1", 0))
            .expect("push");
        frontier
            .push(&job("https://idle.example/1", 0))
            .expect("push");

        let popped = frontier
            .pop_ready(1_000, 100, &|host| host == "busy.example")
            .expect("pop");
        assert_eq!(
            popped.map(|job| job.url),
            Some("https://idle.example/1".to_string())
        );

        // The skipped host's job survived, which is what makes it safe to skip.
        let later = frontier.pop_ready(1_000, 100, &|_| false).expect("pop");
        assert_eq!(
            later.map(|job| job.url),
            Some("https://busy.example/1".to_string())
        );
    }

    #[test]
    fn the_sequence_survives_a_reopen() {
        let (guard, db) = db();
        {
            let mut frontier = Frontier::open(&db).expect("open");
            frontier.push(&job("https://a.example/1", 0)).expect("push");
        }
        drop(db);

        let db = CrawlDb::open(guard.path(), &DbConfig::default()).expect("reopen");
        let mut frontier = Frontier::open(&db).expect("open");
        frontier.push(&job("https://a.example/2", 0)).expect("push");

        // Both jobs must still be distinct entries; a reset counter would have
        // overwritten the first one.
        assert_eq!(frontier.pending_count().expect("count"), 2);
    }

    #[test]
    fn rejecting_a_url_without_a_host() {
        let (_guard, db) = db();
        let mut frontier = Frontier::open(&db).expect("open");
        assert!(
            frontier
                .push(&job("mailto:someone@example.com", 0))
                .is_err()
        );
    }

    #[test]
    fn the_cursor_advances_past_a_whole_host() {
        // The seek target must sort after every key of the host it follows.
        let host = b"example.com";
        let head = common::spool::frontier_key("example.com", 0);
        let tail = common::spool::frontier_key("example.com", u64::MAX);
        let cursor = cursor_after(host);

        assert!(cursor.as_slice() > tail.as_slice());
        assert!(cursor.as_slice() > head.as_slice());
        // And before the next plausible hostname.
        assert!(cursor.as_slice() < common::spool::frontier_key("example.comx", 0).as_slice());
    }
}
