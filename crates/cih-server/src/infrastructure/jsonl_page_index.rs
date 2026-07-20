//! Byte-offset index for paged JSONL artifact reads (Milestone 5, "avoid full
//! scans for resource pages").
//!
//! Paging a `nodes.jsonl` by re-scanning from byte zero costs O(offset) parses
//! per page, so walking a whole resource is quadratic in file size. Measured on
//! the 500k-node reference fixture (50k community records, page size 100), that
//! was 0.04 ms for the first page but 15.4 ms for the tail — and ~4 s of pure
//! rescanning to page through once.
//!
//! This module keeps one `Vec<u64>` of record start offsets per (file, kind),
//! so a page seeks straight to its first record and parses only its own window.
//! The index is built once per file version, shared across callers by the
//! single-flight [`MtimeCache`], and bounded by the same retention discipline as
//! every other cache here (entry cap, idle TTL, weight budget).
//!
//! Callers run inside the heavy blocking lane, so this API is synchronous.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::sync::OnceLock;
use std::time::SystemTime;

use serde::Deserialize;

use super::cache::mtime::{CacheLimits, MtimeCache};

/// Start offsets of every record in one JSONL file whose `kind` equals the
/// indexed label, in file order, plus the freshness token they were built from.
pub(crate) struct KindOffsetIndex {
    offsets: Vec<u64>,
    file_len: u64,
    modified: Option<SystemTime>,
}

impl KindOffsetIndex {
    /// Number of records matching the indexed kind. Available for a future
    /// `total` field on paged responses; currently exercised by tests only.
    #[cfg(test)]
    pub(crate) fn matches(&self) -> usize {
        self.offsets.len()
    }

    fn weight_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.offsets.capacity() * std::mem::size_of::<u64>()
    }

    fn is_fresh(&self, current: &FileToken) -> bool {
        self.file_len == current.len && self.modified == current.modified
    }
}

/// Freshness token: both length and mtime, so a same-second rewrite of a
/// different size is still detected.
struct FileToken {
    len: u64,
    modified: Option<SystemTime>,
}

fn file_token(path: &Path) -> std::io::Result<FileToken> {
    let metadata = std::fs::metadata(path)?;
    Ok(FileToken {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// Only the discriminating field, so index building does not allocate a full
/// `serde_json::Value` tree per line.
#[derive(Deserialize)]
struct KindOnly<'a> {
    #[serde(borrow, default)]
    kind: Option<&'a str>,
}

/// Cheap necessary-condition prefilter: a record whose `kind` is `label` must
/// contain the label text somewhere. `serde_json` never escapes plain ASCII, so
/// engine-written artifacts always satisfy this, and skipping the parse for
/// non-candidate lines is what keeps index building near I/O speed.
fn might_match(line: &str, label: &str) -> bool {
    line.contains(label)
}

fn kind_is(line: &str, label: &str) -> bool {
    serde_json::from_str::<KindOnly>(line)
        .ok()
        .and_then(|record| record.kind)
        .is_some_and(|kind| kind == label)
}

/// Process-wide index cache. Keyed by file path *and* kind label, since one
/// `nodes.jsonl` is paged under several kinds.
fn index_cache() -> &'static MtimeCache<KindOffsetIndex> {
    static CACHE: OnceLock<MtimeCache<KindOffsetIndex>> = OnceLock::new();
    CACHE.get_or_init(|| MtimeCache::with_limits(CacheLimits::resource_index_from_env()))
}

fn cache_key(path: &Path, label: &str) -> String {
    // `\u{1}` cannot occur in a path or a node-kind label, so the composite key
    // is unambiguous.
    format!("{}\u{1}{label}", path.display())
}

/// Read one page of records whose `kind` equals `label`: the records at
/// `[offset, offset + candidate_limit)` in file order, parsed as JSON values.
///
/// Returns fewer records only when the file has fewer matches. Behaviour is
/// identical to a full scan; only the cost differs.
pub(crate) fn page_records(
    path: &Path,
    label: &str,
    offset: usize,
    candidate_limit: usize,
) -> std::io::Result<Vec<serde_json::Value>> {
    let index = load_index(path, label)?;
    if candidate_limit == 0 || offset >= index.offsets.len() {
        return Ok(Vec::new());
    }
    let wanted = candidate_limit.min(index.offsets.len() - offset);
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(index.offsets[offset]))?;
    let mut reader = BufReader::new(file);
    let mut items = Vec::with_capacity(wanted);
    let mut line = String::new();
    while items.len() < wanted {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if !might_match(&line, label) {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if value.get("kind").and_then(|kind| kind.as_str()) == Some(label) {
                items.push(value);
            }
        }
    }
    Ok(items)
}

/// Get-or-build the offset index for `(path, label)`.
pub(crate) fn load_index(
    path: &Path,
    label: &str,
) -> std::io::Result<std::sync::Arc<KindOffsetIndex>> {
    let token = file_token(path)?;
    index_cache().get_or_load_weighted(
        &cache_key(path, label),
        |index| index.is_fresh(&token),
        || build_index(path, label),
        KindOffsetIndex::weight_bytes,
    )
}

fn build_index(path: &Path, label: &str) -> std::io::Result<KindOffsetIndex> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let mut reader = BufReader::new(file);
    let mut offsets = Vec::new();
    let mut position = 0u64;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }
        if might_match(&line, label) && kind_is(&line, label) {
            offsets.push(position);
        }
        position += read as u64;
    }
    offsets.shrink_to_fit();
    Ok(KindOffsetIndex {
        offsets,
        file_len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A fixture where matching records are interleaved with other kinds, so a
    /// naive "records are contiguous" implementation would fail.
    fn fixture(dir: &Path, communities: usize) -> std::path::PathBuf {
        let path = dir.join("nodes.jsonl");
        let mut file = File::create(&path).unwrap();
        for index in 0..communities {
            writeln!(
                file,
                r#"{{"id":"Method:m{index}","kind":"Method","name":"m{index}"}}"#
            )
            .unwrap();
            writeln!(
                file,
                r#"{{"id":"Community:c{index}","kind":"Community","name":"c{index}"}}"#
            )
            .unwrap();
        }
        path
    }

    #[test]
    fn pages_match_a_full_scan_at_every_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), 25);
        let all = page_records(&path, "Community", 0, 1_000).unwrap();
        assert_eq!(all.len(), 25);
        for (index, item) in all.iter().enumerate() {
            assert_eq!(item["id"], format!("Community:c{index}"));
        }
        // Every window equals the corresponding slice of the full scan.
        for offset in 0..25 {
            let page = page_records(&path, "Community", offset, 7).unwrap();
            let expected = &all[offset..(offset + 7).min(all.len())];
            assert_eq!(page, expected, "offset {offset}");
        }
        // Past the end is an empty page, not an error.
        assert!(page_records(&path, "Community", 25, 10).unwrap().is_empty());
        assert!(page_records(&path, "Community", 99, 10).unwrap().is_empty());
        assert!(page_records(&path, "Community", 0, 0).unwrap().is_empty());
        // A kind with no records indexes empty.
        assert!(page_records(&path, "Route", 0, 10).unwrap().is_empty());
    }

    #[test]
    fn index_counts_only_the_requested_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), 10);
        assert_eq!(load_index(&path, "Community").unwrap().matches(), 10);
        assert_eq!(load_index(&path, "Method").unwrap().matches(), 10);
        assert_eq!(load_index(&path, "Route").unwrap().matches(), 0);
    }

    /// The index is a cache, so a rewritten file must not be served from a
    /// stale index — pages have to follow the new content.
    #[test]
    fn rewriting_the_file_rebuilds_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), 5);
        assert_eq!(page_records(&path, "Community", 0, 100).unwrap().len(), 5);

        let mut file = File::create(&path).unwrap();
        for index in 0..9 {
            writeln!(
                file,
                r#"{{"id":"Community:r{index}","kind":"Community","name":"r{index}"}}"#
            )
            .unwrap();
        }
        drop(file);

        let page = page_records(&path, "Community", 0, 100).unwrap();
        assert_eq!(page.len(), 9, "stale index served after rewrite");
        assert_eq!(page[0]["id"], "Community:r0");
    }

    #[test]
    fn malformed_and_unrelated_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nodes.jsonl");
        let mut file = File::create(&path).unwrap();
        writeln!(file, "not json at all").unwrap();
        writeln!(file, r#"{{"id":"Community:a","kind":"Community"}}"#).unwrap();
        writeln!(file, r#"{{"kind":"Method","name":"Community-ish"}}"#).unwrap();
        writeln!(file, r#"{{"id":"Community:b","kind":"Community"}}"#).unwrap();
        drop(file);

        let page = page_records(&path, "Community", 0, 10).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0]["id"], "Community:a");
        assert_eq!(page[1]["id"], "Community:b");
    }

    #[test]
    fn missing_file_is_an_error_not_an_empty_page() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.jsonl");
        assert!(page_records(&missing, "Community", 0, 10).is_err());
    }
}
