//! Reading sealed spool segments.
//!
//! Only `.pdoc` files are ever read. A segment is written as `.open` and renamed
//! on completion, so the rename is what tells this side that the content is
//! complete and fsynced. Reading a segment that is still being appended to would
//! mean indexing a truncated frame.

use std::fs::{self, File};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use common::Document;

use crate::IndexerError;

/// Lists the sealed segments in a directory, in crawl order.
///
/// Sorted by the numeric sequence embedded in the filename rather than by name:
/// the names are zero-padded so the two agree, but parsing the number means the
/// ordering survives a change to the padding.
pub fn sealed_segments(directory: &Path) -> Result<Vec<PathBuf>, IndexerError> {
    let mut segments = Vec::new();

    if !directory.exists() {
        return Ok(segments);
    }

    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if common::spool::is_sealed(name) {
            segments.push(path);
        }
    }

    segments.sort_by_key(|path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .and_then(common::spool::parse_segment_sequence)
            .unwrap_or(u64::MAX)
    });

    Ok(segments)
}

/// Streams a segment's documents to `visit`, returning how many were read.
///
/// Streamed rather than collected into a `Vec` on purpose: a segment is several
/// megabytes on disk and expands further once decoded, and the whole point of
/// this design is that nothing in the pipeline holds a whole crawl's worth of
/// documents in memory at once.
pub fn for_each_document<F>(path: &Path, mut visit: F) -> Result<usize, IndexerError>
where
    F: FnMut(Document) -> Result<(), IndexerError>,
{
    let reader = BufReader::new(File::open(path)?);
    let mut reader = reader;
    let mut count = 0usize;

    while let Some(document) = common::spool::read_frame(&mut reader)? {
        visit(document)?;
        count += 1;
    }

    Ok(count)
}

/// Removes a processed segment, or moves it aside if an archive is configured.
///
/// Called only *after* the documents have been committed, which is what makes a
/// crash replay work instead of losing pages.
pub fn retire(path: &Path, archive_dir: Option<&Path>) -> Result<(), IndexerError> {
    match archive_dir {
        Some(directory) => {
            fs::create_dir_all(directory)?;
            let name = path
                .file_name()
                .ok_or_else(|| std::io::Error::other("segment path has no file name"))?;
            fs::rename(path, directory.join(name))?;
        }
        None => fs::remove_file(path)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::spool::{open_segment_name, sealed_segment_name, write_frame};
    use std::io::Write;

    fn write_segment(directory: &Path, sequence: u64, urls: &[&str]) -> PathBuf {
        let path = directory.join(sealed_segment_name(sequence));
        let mut file = File::create(&path).expect("create segment");
        for url in urls {
            write_frame(&mut file, &Document::new(*url, "t", "b", 0, Vec::new())).expect("frame");
        }
        file.flush().expect("flush");
        path
    }

    #[test]
    fn only_sealed_segments_are_listed() {
        let directory = tempfile::tempdir().expect("tempdir");
        write_segment(directory.path(), 1, &["https://example.com/a"]);

        // A segment still being appended to must be invisible to this side.
        let open = directory.path().join(open_segment_name(2));
        File::create(&open).expect("create open segment");
        fs::write(directory.path().join("unrelated.txt"), b"hello").expect("write");

        let found = sealed_segments(directory.path()).expect("list");
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with(sealed_segment_name(1)));
    }

    #[test]
    fn segments_are_ordered_by_sequence_not_by_name() {
        let directory = tempfile::tempdir().expect("tempdir");
        // Written out of order on purpose.
        write_segment(directory.path(), 10, &["https://example.com/c"]);
        write_segment(directory.path(), 2, &["https://example.com/a"]);
        write_segment(directory.path(), 1_000, &["https://example.com/z"]);

        let found = sealed_segments(directory.path()).expect("list");
        let sequences: Vec<u64> = found
            .iter()
            .map(|path| {
                common::spool::parse_segment_sequence(path.file_name().unwrap().to_str().unwrap())
                    .expect("sequence")
            })
            .collect();

        assert_eq!(sequences, vec![2, 10, 1_000]);
    }

    #[test]
    fn a_missing_directory_is_empty_rather_than_an_error() {
        // The indexer legitimately starts before the crawler has written anything.
        let directory = tempfile::tempdir().expect("tempdir");
        let found = sealed_segments(&directory.path().join("does-not-exist")).expect("list");
        assert!(found.is_empty());
    }

    #[test]
    fn documents_are_streamed_in_order() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = write_segment(
            directory.path(),
            1,
            &["https://example.com/a", "https://example.com/b"],
        );

        let mut seen = Vec::new();
        let count = for_each_document(&path, |document| {
            seen.push(document.url);
            Ok(())
        })
        .expect("read");

        assert_eq!(count, 2);
        assert_eq!(seen, vec!["https://example.com/a", "https://example.com/b"]);
    }

    #[test]
    fn a_visit_that_fails_stops_the_read() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = write_segment(
            directory.path(),
            1,
            &["https://example.com/a", "https://example.com/b"],
        );

        let mut visits = 0;
        let result = for_each_document(&path, |_document| {
            visits += 1;
            Err(IndexerError::Io(std::io::Error::other(
                "index write failed",
            )))
        });

        assert!(result.is_err());
        assert_eq!(
            visits, 1,
            "a failed write must not keep consuming the segment"
        );
    }

    #[test]
    fn retiring_deletes_the_segment_by_default() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = write_segment(directory.path(), 1, &["https://example.com/a"]);

        retire(&path, None).expect("retire");

        assert!(!path.exists());
    }

    #[test]
    fn retiring_can_move_the_segment_aside_instead() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = write_segment(directory.path(), 1, &["https://example.com/a"]);
        let archive = directory.path().join("done");

        retire(&path, Some(&archive)).expect("retire");

        assert!(!path.exists());
        assert!(archive.join(sealed_segment_name(1)).exists());
    }
}
