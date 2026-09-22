//! The hand-off format between the crawler and the indexer.
//!
//! ## Why a spool directory and not a database
//!
//! The obvious design is a queue inside the crawler's RocksDB. It cannot work:
//! RocksDB takes an exclusive lock on its directory, so the crawler and the
//! indexer cannot both hold write access at the same time. A read-only second
//! instance is not an escape either, because the indexer has to *remove* entries
//! once they are committed, and a read-only instance cannot delete.
//!
//! A spool directory sidesteps the whole problem. It is just files, so any number
//! of processes may read and write it safely, and it gives three properties that
//! a naive queue would not:
//!
//! * **Crash replay.** A segment is deleted only after its documents are
//!   committed, so a crash replays work instead of losing it.
//! * **Sealed reads.** The crawler appends to a `.open` file and rotates by
//!   renaming it to `.pdoc`. The indexer only ever reads `.pdoc` files, so it can
//!   never see a half-written frame.
//! * **Batching for free.** One segment is one commit, so segment size is the
//!   knob that trades index write amplification against indexing latency.
//!
//! ## Frame format
//!
//! A segment is a sequence of frames: a little-endian `u32` byte length followed
//! by that many bytes of postcard-encoded [`Document`]. Postcard is not
//! self-describing, which is the point -- a multi-kilobyte page body dominates
//! the value rather than the encoding's field names.

use std::io::{ErrorKind, Read, Write};

use serde::{Deserialize, Serialize};

use crate::Document;

/// Byte length of a frame header.
pub const FRAME_HEADER_BYTES: usize = 4;

/// Default segment size before the crawler rotates to a new one.
///
/// Large enough that per-file overhead and per-commit cost are amortised, small
/// enough that a crash replays seconds rather than minutes of crawling.
pub const DEFAULT_SEGMENT_BYTES: u64 = 8 * 1024 * 1024;

/// Suffix of a segment still being appended to.
pub const OPEN_SUFFIX: &str = ".open";
/// Suffix of a segment that is complete and safe to read.
pub const SEALED_SUFFIX: &str = ".pdoc";

/// A URL waiting to be fetched, as stored in the frontier.
///
/// Lives next to the framing code because the frontier's key layout is derived
/// from the URL's host, and both sides of that decision belong together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrawlJob {
    /// Absolute URL to fetch.
    pub url: String,
    /// Link depth from the seed that produced this URL.
    pub depth: u32,
}

/// Frontier key for a job: `host` + `0x00` + big-endian sequence.
///
/// The separator is safe because a hostname can never contain a NUL byte, and it
/// gives the ordering the frontier needs for free: iterating forwards yields
/// every URL of one host before the next host's, in insertion order, with no
/// separate in-memory index. Without it, `example.com` would be a prefix of
/// `example.com.evil` and the two hosts' entries would interleave.
pub fn frontier_key(host: &str, sequence: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(host.len() + 9);
    key.extend_from_slice(host.as_bytes());
    key.push(0);
    key.extend_from_slice(&sequence.to_be_bytes());
    key
}

/// Recovers the host from a [`frontier_key`].
pub fn frontier_key_host(key: &[u8]) -> Option<&[u8]> {
    let separator = key.iter().position(|byte| *byte == 0)?;
    Some(&key[..separator])
}

/// Errors from reading or writing spool frames.
#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    /// The underlying stream failed.
    #[error("spool io error: {0}")]
    Io(#[from] std::io::Error),
    /// A frame's payload could not be encoded.
    #[error("could not encode spool frame: {0}")]
    Encode(#[source] postcard::Error),
    /// A frame's payload could not be decoded.
    #[error("could not decode spool frame: {0}")]
    Decode(#[source] postcard::Error),
    /// A frame header or payload ended early.
    #[error("truncated spool frame: expected {expected} bytes, got {found}")]
    Truncated {
        /// Bytes the frame header promised.
        expected: usize,
        /// Bytes actually available.
        found: usize,
    },
}

/// Filename of the segment a crawler is currently appending to.
///
/// Zero-padded so that lexical order equals numeric order: the indexer sorts
/// filenames and gets crawl order without parsing anything.
pub fn open_segment_name(sequence: u64) -> String {
    format!("segment-{sequence:012}{OPEN_SUFFIX}")
}

/// Filename of a complete, readable segment.
pub fn sealed_segment_name(sequence: u64) -> String {
    format!("segment-{sequence:012}{SEALED_SUFFIX}")
}

/// Whether a filename denotes a complete segment.
pub fn is_sealed(name: &str) -> bool {
    name.ends_with(SEALED_SUFFIX)
}

/// Recovers the sequence number from a segment filename, sealed or open.
pub fn parse_segment_sequence(name: &str) -> Option<u64> {
    let stem = name
        .strip_suffix(SEALED_SUFFIX)
        .or_else(|| name.strip_suffix(OPEN_SUFFIX))?;
    stem.strip_prefix("segment-")?.parse().ok()
}

/// Writes one length-prefixed frame.
pub fn write_frame<W: Write>(writer: &mut W, document: &Document) -> Result<usize, SpoolError> {
    let payload = postcard::to_allocvec(document).map_err(SpoolError::Encode)?;
    let length = u32::try_from(payload.len()).map_err(|_| SpoolError::Truncated {
        expected: u32::MAX as usize,
        found: payload.len(),
    })?;

    writer.write_all(&length.to_le_bytes())?;
    writer.write_all(&payload)?;
    Ok(FRAME_HEADER_BYTES + payload.len())
}

/// Reads one frame, or `Ok(None)` at a clean end of segment.
///
/// A truncated frame is an error rather than a silent stop. A segment is sealed
/// before it is read, so truncation means real corruption, and quietly dropping
/// documents would hide it.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Document>, SpoolError> {
    let mut header = [0u8; FRAME_HEADER_BYTES];
    if !read_exact_or_eof(reader, &mut header)? {
        return Ok(None);
    }

    let length = u32::from_le_bytes(header) as usize;
    let mut payload = vec![0u8; length];
    if !read_exact_or_eof(reader, &mut payload)? {
        return Err(SpoolError::Truncated {
            expected: length,
            found: 0,
        });
    }

    let document = postcard::from_bytes(&payload).map_err(SpoolError::Decode)?;
    Ok(Some(document))
}

/// Fills `buffer` entirely, reporting whether any bytes were available at all.
fn read_exact_or_eof<R: Read>(reader: &mut R, buffer: &mut [u8]) -> Result<bool, SpoolError> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(SpoolError::Truncated {
                    expected: buffer.len(),
                    found: filled,
                });
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(SpoolError::Io(error)),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(url: &str) -> Document {
        Document::new(
            url,
            "a title",
            "a body long enough to span a frame",
            1,
            Vec::new(),
        )
    }

    #[test]
    fn frames_round_trip_in_order() {
        let mut buffer = Vec::new();
        for index in 0..5 {
            write_frame(
                &mut buffer,
                &document(&format!("https://example.com/{index}")),
            )
            .expect("write");
        }

        let mut reader = buffer.as_slice();
        let mut urls = Vec::new();
        while let Some(document) = read_frame(&mut reader).expect("read") {
            urls.push(document.url);
        }

        assert_eq!(
            urls,
            (0..5)
                .map(|i| format!("https://example.com/{i}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_empty_segment_reads_as_a_clean_end() {
        let mut reader: &[u8] = &[];
        assert!(
            read_frame(&mut reader)
                .expect("eof is not an error")
                .is_none()
        );
    }

    #[test]
    fn a_truncated_header_is_an_error_not_a_silent_stop() {
        // Two bytes of a four-byte header. Silently stopping here would drop
        // whatever the seal process had already promised.
        let mut reader: &[u8] = &[1, 0];
        assert!(read_frame(&mut reader).is_err());
    }

    #[test]
    fn a_truncated_payload_is_an_error() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &document("https://example.com/a")).expect("write");
        buffer.truncate(buffer.len() - 1);

        let mut reader = buffer.as_slice();
        assert!(read_frame(&mut reader).is_err());
    }

    #[test]
    fn segment_names_sort_in_crawl_order() {
        let mut sorted = [10u64, 2, 1_000].map(sealed_segment_name).to_vec();
        sorted.sort();
        assert_eq!(sorted, [2u64, 10, 1_000].map(sealed_segment_name).to_vec());
    }

    #[test]
    fn frontier_keys_group_by_host_and_keep_order_within_a_host() {
        let a1 = frontier_key("a.example", 1);
        let a2 = frontier_key("a.example", 2);
        let b1 = frontier_key("b.example", 1);

        // Every URL of one host comes before the next host's, which is what lets
        // the frontier iterate per-host FIFO straight off the key order.
        assert!(a1 < a2);
        assert!(a2 < b1);
    }

    #[test]
    fn a_host_that_is_a_prefix_of_another_does_not_collide() {
        let short = frontier_key("short.example", 5);
        let long = frontier_key("short.example.more", 1);

        assert_ne!(frontier_key_host(&short), frontier_key_host(&long));
        assert!(short < long);
    }

    #[test]
    fn frontier_keys_round_trip_their_host() {
        assert_eq!(
            frontier_key_host(&frontier_key("example.com", 7)),
            Some(&b"example.com"[..])
        );
        assert_eq!(frontier_key_host(&[1, 2, 3]), None);
    }

    #[test]
    fn segment_names_round_trip_their_sequence() {
        assert_eq!(parse_segment_sequence(&sealed_segment_name(42)), Some(42));
        assert_eq!(parse_segment_sequence(&open_segment_name(42)), Some(42));
        assert_eq!(parse_segment_sequence("not-a-segment.txt"), None);
    }

    #[test]
    fn only_sealed_segments_are_readable() {
        // This is the property that stops the indexer from reading a frame the
        // crawler is still in the middle of writing.
        assert!(is_sealed(&sealed_segment_name(1)));
        assert!(!is_sealed(&open_segment_name(1)));
        assert!(!is_sealed("segment-000000000001"));
    }
}
