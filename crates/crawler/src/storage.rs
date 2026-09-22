//! Fetched-page persistence: WARC on disk, plus the hand-off to indexing.
//!
//! Two things are written for every page:
//!
//! * a **WARC record**, zstd-compressed, which is the durable archive of the raw
//!   response. It is the thing that lets the index be rebuilt without crawling
//!   again.
//! * a **spool frame**, holding the pre-extracted [`Document`], which the indexer
//!   drains.
//!
//! The spool exists rather than the indexer reading WARC directly because
//! re-extracting text from WARC would duplicate the parser's work and re-pay its
//! cost on every reindex.
//!
//! ## Sealing
//!
//! A spool segment is written as `segment-NNNNNNNNNNNN.open` and rotated by
//! renaming it to `.pdoc`. The indexer only reads `.pdoc`, so it can never see a
//! half-written frame. Rotation happens on a size threshold, and on shutdown.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use common::Document;
use common::spool::{self, DEFAULT_SEGMENT_BYTES};

use crate::CrawlError;
use crate::fetcher::FetchedPage;

/// Persistence policy.
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Directory for WARC output.
    pub warc_dir: PathBuf,
    /// Directory for spool segments.
    pub spool_dir: PathBuf,
    /// zstd compression level. Levels past ~9 buy little on HTML and cost
    /// noticeably more CPU.
    pub compression_level: i32,
    /// Roll to a new WARC file once the current one exceeds this size.
    pub warc_max_bytes: u64,
    /// Roll to a new spool segment once the current one exceeds this size.
    pub segment_bytes: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            warc_dir: PathBuf::from("data/warc"),
            spool_dir: PathBuf::from("data/spool"),
            compression_level: 6,
            warc_max_bytes: 1024 * 1024 * 1024,
            segment_bytes: DEFAULT_SEGMENT_BYTES,
        }
    }
}

/// Formats a unix timestamp as an ISO 8601 UTC timestamp, as WARC requires.
pub fn iso8601(timestamp_seconds: i64) -> String {
    let days = timestamp_seconds.div_euclid(86_400);
    let seconds_of_day = timestamp_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        seconds_of_day / 3_600,
        (seconds_of_day % 3_600) / 60,
        seconds_of_day % 60,
    );

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Converts days since the unix epoch into a civil date.
///
/// Howard Hinnant's `civil_from_days`, which is exact for the whole range we care
/// about and needs no calendar library.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;

    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// A stable, unique record identifier.
///
/// WARC only requires the `WARC-Record-ID` to be a unique URI, not a real UUID,
/// so this is derived from the URL and timestamp rather than pulling in a UUID
/// dependency. Being deterministic is a bonus: re-crawling the same URL at the
/// same second reproduces the same record id.
pub fn record_id(url: &str, fetched_at: i64) -> String {
    let high = common::content_hash(&format!("{url}\u{1}{fetched_at}"));
    let low = common::content_hash(&format!("{fetched_at}\u{1}{url}"));
    format!(
        "urn:uuid:{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (high >> 32) as u32,
        (high >> 16) as u16,
        high as u16,
        (low >> 48) as u16,
        low & 0x0000_ffff_ffff_ffff
    )
}

/// A spool segment being appended to.
struct SpoolSegment {
    writer: BufWriter<File>,
    open_path: PathBuf,
    sealed_path: PathBuf,
    bytes: u64,
}

impl SpoolSegment {
    fn create(directory: &Path, sequence: u64) -> Result<Self, CrawlError> {
        let open_path = directory.join(spool::open_segment_name(sequence));
        let sealed_path = directory.join(spool::sealed_segment_name(sequence));
        let file = File::create(&open_path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            open_path,
            sealed_path,
            bytes: 0,
        })
    }

    fn write(&mut self, document: &Document) -> Result<(), CrawlError> {
        self.bytes += spool::write_frame(&mut self.writer, document)? as u64;
        Ok(())
    }

    /// Flushes, fsyncs and renames the segment into its readable name.
    ///
    /// The fsync before the rename is what makes the rename a reliable signal:
    /// otherwise a crash could leave a `.pdoc` file whose contents never reached
    /// the disk.
    fn seal(self) -> Result<(), CrawlError> {
        let file = self
            .writer
            .into_inner()
            .map_err(|error| CrawlError::Io(error.into_error()))?;
        file.sync_all()?;
        drop(file);
        fs::rename(&self.open_path, &self.sealed_path)?;
        Ok(())
    }
}

/// Counters describing what has been written.
#[derive(Debug, Clone, Default)]
pub struct ArchiveStats {
    /// Documents handed to the indexer.
    pub documents: u64,
    /// Spool segments sealed and ready to drain.
    pub sealed_segments: u64,
    /// WARC records written.
    pub warc_records: u64,
    /// Uncompressed bytes handed to the compressor.
    pub warc_bytes: u64,
    /// WARC files started.
    pub warc_files: u64,
}

/// Writes WARC archives and spool segments.
pub struct ArchiveWriter {
    config: StorageConfig,
    warc: Option<zstd::stream::write::Encoder<'static, BufWriter<File>>>,
    warc_sequence: u64,
    warc_path: PathBuf,
    segment: Option<SpoolSegment>,
    segment_sequence: u64,
    stats: ArchiveStats,
}

impl ArchiveWriter {
    /// Opens the archive, creating both directories and resuming numbering.
    pub fn open(config: StorageConfig) -> Result<Self, CrawlError> {
        fs::create_dir_all(&config.warc_dir)?;
        fs::create_dir_all(&config.spool_dir)?;

        let mut writer = Self {
            warc_sequence: next_warc_sequence(&config.warc_dir)?,
            segment_sequence: next_spool_sequence(&config.spool_dir)?,
            warc: None,
            warc_path: PathBuf::new(),
            segment: None,
            stats: ArchiveStats::default(),
            config,
        };
        writer.open_warc()?;
        writer.open_segment()?;
        Ok(writer)
    }

    /// Appends one page: a WARC record and a spool frame.
    pub fn write_page(
        &mut self,
        page: &FetchedPage,
        document: &Document,
    ) -> Result<(), CrawlError> {
        self.write_warc_record(page, document)?;

        let segment = self
            .segment
            .as_mut()
            .ok_or(CrawlError::NotImplemented("archive is sealed"))?;
        segment.write(document)?;
        self.stats.documents += 1;

        if segment.bytes >= self.config.segment_bytes {
            self.seal_segment()?;
            self.open_segment()?;
        }
        Ok(())
    }

    /// Seals the current spool segment and finishes the current WARC file.
    pub fn seal(&mut self) -> Result<(), CrawlError> {
        self.seal_segment()?;
        self.finish_warc()?;
        Ok(())
    }

    /// Counters for reporting.
    pub fn stats(&self) -> &ArchiveStats {
        &self.stats
    }

    /// Path of the WARC file currently being written.
    pub fn current_warc_path(&self) -> &Path {
        &self.warc_path
    }

    fn open_warc(&mut self) -> Result<(), CrawlError> {
        let path = self
            .config
            .warc_dir
            .join(format!("crawl-{:08}.warc.zst", self.warc_sequence));
        self.warc_sequence += 1;

        let file = File::create(&path)?;
        let mut encoder =
            zstd::stream::write::Encoder::new(BufWriter::new(file), self.config.compression_level)?;
        encoder.include_checksum(true)?;

        let header = warcinfo_record(
            &path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        encoder.write_all(header.as_bytes())?;

        self.stats.warc_bytes += header.len() as u64;
        self.stats.warc_records += 1;
        self.stats.warc_files += 1;
        self.warc = Some(encoder);
        self.warc_path = path;
        Ok(())
    }

    fn finish_warc(&mut self) -> Result<(), CrawlError> {
        if let Some(encoder) = self.warc.take() {
            let mut buffered = encoder.finish()?;
            buffered.flush()?;
            let file = buffered
                .into_inner()
                .map_err(|error| CrawlError::Io(error.into_error()))?;
            file.sync_all()?;
        }
        Ok(())
    }

    fn write_warc_record(
        &mut self,
        page: &FetchedPage,
        document: &Document,
    ) -> Result<(), CrawlError> {
        let payload = http_payload(page);
        let record = format!(
            "WARC/1.1\r\n\
             WARC-Type: response\r\n\
             WARC-Record-ID: <{record_id}>\r\n\
             WARC-Date: {date}\r\n\
             WARC-Target-URI: {url}\r\n\
             Content-Type: application/http; msgtype=response\r\n\
             Content-Length: {length}\r\n\
             \r\n",
            record_id = record_id(&document.url, document.fetched_at),
            date = iso8601(document.fetched_at),
            url = document.url,
            length = payload.len(),
        );

        let encoder = self
            .warc
            .as_mut()
            .ok_or(CrawlError::NotImplemented("warc file is closed"))?;
        encoder.write_all(record.as_bytes())?;
        encoder.write_all(payload.as_bytes())?;
        encoder.write_all(b"\r\n\r\n")?;

        self.stats.warc_bytes += (record.len() + payload.len() + 4) as u64;
        self.stats.warc_records += 1;

        if self.stats.warc_bytes >= self.config.warc_max_bytes {
            self.finish_warc()?;
            self.open_warc()?;
        }
        Ok(())
    }

    fn open_segment(&mut self) -> Result<(), CrawlError> {
        self.segment = Some(SpoolSegment::create(
            &self.config.spool_dir,
            self.segment_sequence,
        )?);
        self.segment_sequence += 1;
        Ok(())
    }

    fn seal_segment(&mut self) -> Result<(), CrawlError> {
        if let Some(segment) = self.segment.take() {
            segment.seal()?;
            self.stats.sealed_segments += 1;
        }
        Ok(())
    }
}

impl Drop for ArchiveWriter {
    fn drop(&mut self) {
        // Best effort: an error here cannot be reported usefully, and the spool
        // frame format is designed so that an unsealed segment is simply ignored
        // rather than misread.
        let _ = self.seal_segment();
        let _ = self.finish_warc();
    }
}

/// The stored HTTP response: a status line, headers and the raw body.
fn http_payload(page: &FetchedPage) -> String {
    let content_type = page
        .content_type
        .as_deref()
        .unwrap_or("text/html; charset=utf-8");

    format!(
        "HTTP/1.1 {} \r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {length}\r\n\
         \r\n\
         {body}",
        page.status,
        length = page.body.len(),
        body = page.body,
    )
}

/// The `warcinfo` record written at the head of every WARC file.
fn warcinfo_record(filename: &str) -> String {
    let fields = format!(
        "software: MJLsearchBot/{}\r\nformat: WARC File Format 1.1\r\n",
        env!("CARGO_PKG_VERSION")
    );
    format!(
        "WARC/1.1\r\n\
         WARC-Type: warcinfo\r\n\
         WARC-Record-ID: <urn:uuid:00000000-0000-0000-0000-{:012x}>\r\n\
         WARC-Date: {date}\r\n\
         WARC-Filename: {filename}\r\n\
         Content-Type: application/warc-fields\r\n\
         Content-Length: {length}\r\n\
         \r\n{fields}\r\n\r\n",
        common::content_hash(filename) & 0x0000_ffff_ffff_ffff,
        date = iso8601(now_seconds()),
        length = fields.len(),
    )
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn next_warc_sequence(directory: &Path) -> Result<u64, CrawlError> {
    let mut highest = 0u64;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(digits) = name
            .strip_prefix("crawl-")
            .and_then(|rest| rest.strip_suffix(".warc.zst"))
        else {
            continue;
        };
        if let Ok(sequence) = digits.parse::<u64>() {
            highest = highest.max(sequence);
        }
    }
    Ok(highest + 1)
}

fn next_spool_sequence(directory: &Path) -> Result<u64, CrawlError> {
    let mut highest = 0u64;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(sequence) = spool::parse_segment_sequence(name) {
            highest = highest.max(sequence);
        }
    }
    Ok(highest + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(directory: &Path) -> StorageConfig {
        StorageConfig {
            warc_dir: directory.join("warc"),
            spool_dir: directory.join("spool"),
            compression_level: 1,
            warc_max_bytes: 1 << 30,
            segment_bytes: 1 << 20,
        }
    }

    fn page(url: &str, body: &str) -> (FetchedPage, Document) {
        let document = Document::new(url, "title", body, 1_700_000_000, Vec::new());
        let page = FetchedPage {
            final_url: url.to_string(),
            status: 200,
            content_type: Some("text/html".to_string()),
            body: format!("<html><body>{body}</body></html>"),
        };
        (page, document)
    }

    #[test]
    fn iso8601_formats_known_instants() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso8601(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day, which is where naive date arithmetic usually breaks.
        assert_eq!(iso8601(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn iso8601_handles_instants_before_the_epoch() {
        assert_eq!(iso8601(-1), "1969-12-31T23:59:59Z");
        assert_eq!(iso8601(-86_400), "1969-12-31T00:00:00Z");
    }

    #[test]
    fn record_ids_are_unique_per_url_and_timestamp_but_stable() {
        let first = record_id("https://example.com/a", 1_700_000_000);
        assert!(first.starts_with("urn:uuid:"));
        assert_eq!(first, record_id("https://example.com/a", 1_700_000_000));
        assert_ne!(first, record_id("https://example.com/b", 1_700_000_000));
        assert_ne!(first, record_id("https://example.com/a", 1_700_000_001));
    }

    #[test]
    fn a_written_page_is_readable_from_the_sealed_segment() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (page, document) = page("https://example.com/a", "hello");

        {
            let mut archive = ArchiveWriter::open(config(directory.path())).expect("open");
            archive.write_page(&page, &document).expect("write");
            archive.seal().expect("seal");
        }

        let spool_dir = directory.path().join("spool");
        let sealed: Vec<_> = fs::read_dir(&spool_dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| spool::is_sealed(&entry.file_name().to_string_lossy()))
            .collect();
        assert_eq!(
            sealed.len(),
            1,
            "seal must produce exactly one readable segment"
        );

        let file = File::open(sealed[0].path()).expect("open segment");
        let mut reader = std::io::BufReader::new(file);
        let restored = spool::read_frame(&mut reader)
            .expect("read")
            .expect("one frame");
        assert_eq!(restored, document);
        assert!(spool::read_frame(&mut reader).expect("read").is_none());
    }

    #[test]
    fn an_unsealed_segment_is_not_readable() {
        // This is the property that keeps the indexer from reading a frame the
        // crawler is still writing.
        let directory = tempfile::tempdir().expect("tempdir");
        let (page, document) = page("https://example.com/a", "hello");

        let mut archive = ArchiveWriter::open(config(directory.path())).expect("open");
        archive.write_page(&page, &document).expect("write");

        let spool_dir = directory.path().join("spool");
        let names: Vec<String> = fs::read_dir(&spool_dir)
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();

        assert_eq!(names.len(), 1);
        assert!(
            !spool::is_sealed(&names[0]),
            "a segment in progress must not look sealed, found {names:?}"
        );
    }

    #[test]
    fn the_warc_file_holds_the_raw_response_compressed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (page, document) = page("https://example.com/a", "unique-marker-text");

        let warc_path = {
            let mut archive = ArchiveWriter::open(config(directory.path())).expect("open");
            archive.write_page(&page, &document).expect("write");
            let path = archive.current_warc_path().to_path_buf();
            archive.seal().expect("seal");
            path
        };

        assert!(warc_path.exists());
        assert!(warc_path.to_string_lossy().ends_with(".warc.zst"));

        let file = File::open(&warc_path).expect("open warc");
        let mut contents = String::new();
        std::io::Read::read_to_string(
            &mut zstd::stream::read::Decoder::new(file).expect("decoder"),
            &mut contents,
        )
        .expect("decompress");

        assert!(contents.contains("WARC/1.1"));
        assert!(contents.contains("WARC-Type: warcinfo"));
        assert!(contents.contains("WARC-Type: response"));
        assert!(contents.contains("WARC-Target-URI: https://example.com/a"));
        assert!(contents.contains("unique-marker-text"));
    }

    #[test]
    fn spool_rotation_loses_no_documents() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut storage = config(directory.path());
        // Small enough to force several rotations across five documents.
        storage.segment_bytes = 64;
        storage.warc_max_bytes = 1 << 30;

        {
            let mut archive = ArchiveWriter::open(storage).expect("open");
            for index in 0..5 {
                let (page, document) =
                    page(&format!("https://example.com/{index}"), "body text here");
                archive.write_page(&page, &document).expect("write");
            }
            archive.seal().expect("seal");
            assert!(
                archive.stats().sealed_segments >= 2,
                "expected rotation to produce several segments, got {}",
                archive.stats().sealed_segments
            );
        }

        // The property that actually matters: every document written is readable
        // exactly once, whichever segment it landed in. A rotation that dropped or
        // duplicated a document would be invisible without this check.
        let mut urls = Vec::new();
        for entry in fs::read_dir(directory.path().join("spool")).expect("read dir") {
            let path = entry.expect("entry").path();
            if !spool::is_sealed(&path.file_name().unwrap().to_string_lossy()) {
                continue;
            }
            let mut reader = std::io::BufReader::new(File::open(&path).expect("open"));
            while let Some(document) = spool::read_frame(&mut reader).expect("read") {
                urls.push(document.url);
            }
        }

        urls.sort();
        assert_eq!(
            urls.len(),
            5,
            "expected every written document exactly once, got {urls:?}"
        );
        assert_eq!(urls[0], "https://example.com/0");
        assert_eq!(urls[4], "https://example.com/4");
    }

    #[test]
    fn sequence_numbers_resume_after_a_reopen() {
        let directory = tempfile::tempdir().expect("tempdir");
        let storage = config(directory.path());

        let first = {
            let mut archive = ArchiveWriter::open(storage.clone()).expect("open");
            let (page, document) = page("https://example.com/a", "one");
            archive.write_page(&page, &document).expect("write");
            let path = archive.current_warc_path().to_path_buf();
            archive.seal().expect("seal");
            path
        };

        // Restarting must not reuse a name: doing so would silently overwrite
        // archives that the index has not caught up with.
        let second = {
            let archive = ArchiveWriter::open(storage).expect("reopen");
            let path = archive.current_warc_path().to_path_buf();
            drop(archive);
            path
        };

        assert_ne!(first, second);
        assert!(first.exists());
    }

    #[test]
    fn stats_track_what_was_written() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut archive = ArchiveWriter::open(config(directory.path())).expect("open");

        for index in 0..3 {
            let (page, document) = page(&format!("https://example.com/{index}"), "body");
            archive.write_page(&page, &document).expect("write");
        }

        let stats = archive.stats();
        assert_eq!(stats.documents, 3);
        // One warcinfo record plus three responses.
        assert_eq!(stats.warc_records, 4);
        assert_eq!(stats.warc_files, 1);
        assert!(stats.warc_bytes > 0);
    }

    #[test]
    fn default_config_is_sane() {
        let config = StorageConfig::default();
        assert!(config.warc_max_bytes > 0);
        assert_eq!(config.segment_bytes, DEFAULT_SEGMENT_BYTES);
        assert!(
            (1..=9).contains(&config.compression_level),
            "zstd levels above 9 cost far more CPU than they save in bytes"
        );
    }
}
