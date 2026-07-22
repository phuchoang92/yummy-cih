use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cih_core::{NodeId, NodeKind, Range};

use crate::bm25::{IndexedDoc, SearchIndex};

const MAGIC: &[u8; 8] = b"CIHSRCH1";
pub const SEARCH_INDEX_FORMAT_VERSION: u32 = 1;
pub const SEARCH_INDEX_FILE_NAME: &str = "search-index.bin";
const MAX_ARTIFACT_VERSION_BYTES: usize = 4096;
const MAX_STRING_BYTES: usize = 16 * 1024 * 1024;
const MAX_COLLECTION_ITEMS: usize = 100_000_000;
const MAX_PAYLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const SCHEMA_DESCRIPTOR: &str = concat!(
    "cih-search-schema-v1;",
    "tokenizer=ascii-alnum-camel-v1;",
    "fields=kind,name,qualified_name,node_id,file,route,integration,message;",
    "k1=1.2;b=0.75;",
    "representation=interned-files,boxed-postings,no-text,no-doc-freq"
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchIndexSource {
    pub artifact_version: String,
    pub nodes_len: u64,
    pub nodes_modified_secs: u64,
    pub nodes_modified_nanos: u32,
}

impl SearchIndexSource {
    pub fn from_nodes_file(path: &Path, artifact_version: impl Into<String>) -> io::Result<Self> {
        let metadata = fs::metadata(path)?;
        let modified = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Self {
            artifact_version: artifact_version.into(),
            nodes_len: metadata.len(),
            nodes_modified_secs: modified.as_secs(),
            nodes_modified_nanos: modified.subsec_nanos(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchIndexMetadata {
    pub format_version: u32,
    pub schema_fingerprint: [u8; 32],
    pub source: SearchIndexSource,
    pub retained_size_bytes: u64,
    pub payload_len: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchIndexInspection {
    Missing,
    Present(SearchIndexMetadata),
    Invalid(String),
}

#[derive(Debug)]
pub enum SearchIndexLoad {
    Loaded {
        index: Box<SearchIndex>,
        metadata: SearchIndexMetadata,
    },
    Missing,
    Stale(String),
    Corrupt(String),
}

pub fn search_schema_fingerprint() -> [u8; 32] {
    *blake3::hash(SCHEMA_DESCRIPTOR.as_bytes()).as_bytes()
}

pub fn search_index_path(artifacts_dir: &Path) -> PathBuf {
    artifacts_dir.join(SEARCH_INDEX_FILE_NAME)
}

pub fn inspect_search_index(path: &Path) -> io::Result<SearchIndexInspection> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SearchIndexInspection::Missing)
        }
        Err(error) => return Err(error),
    };
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    match read_header(&mut reader) {
        Ok(header) => {
            let payload_start = reader.stream_position()?;
            if payload_start
                .checked_add(header.metadata.payload_len)
                .is_none_or(|expected| expected != file_len)
            {
                return Ok(SearchIndexInspection::Invalid(
                    "payload length does not match file length".into(),
                ));
            }
            Ok(SearchIndexInspection::Present(header.metadata))
        }
        Err(error) => Ok(SearchIndexInspection::Invalid(error.to_string())),
    }
}

pub fn load_search_index(path: &Path, expected: &SearchIndexSource) -> io::Result<SearchIndexLoad> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SearchIndexLoad::Missing)
        }
        Err(error) => return Err(error),
    };
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let header = match read_header(&mut reader) {
        Ok(header) => header,
        Err(error) => return Ok(SearchIndexLoad::Corrupt(error.to_string())),
    };

    if header.metadata.format_version != SEARCH_INDEX_FORMAT_VERSION {
        return Ok(SearchIndexLoad::Stale(format!(
            "format version {} is not supported",
            header.metadata.format_version
        )));
    }
    if header.metadata.schema_fingerprint != search_schema_fingerprint() {
        return Ok(SearchIndexLoad::Stale(
            "search schema fingerprint changed".into(),
        ));
    }
    if &header.metadata.source != expected {
        return Ok(SearchIndexLoad::Stale("source identity changed".into()));
    }
    let payload_start = reader.stream_position()?;
    if payload_start
        .checked_add(header.metadata.payload_len)
        .is_none_or(|expected_len| expected_len != file_len)
    {
        return Ok(SearchIndexLoad::Corrupt(
            "payload length does not match file length".into(),
        ));
    }

    let mut payload = PayloadReader::new(reader, header.metadata.payload_len);
    let index = match decode_index(&mut payload) {
        Ok(index) => index,
        Err(error) => return Ok(SearchIndexLoad::Corrupt(error.to_string())),
    };
    if payload.remaining != 0 {
        return Ok(SearchIndexLoad::Corrupt(
            "payload contains trailing bytes".into(),
        ));
    }
    if payload.checksum() != header.payload_checksum {
        return Ok(SearchIndexLoad::Corrupt("payload checksum mismatch".into()));
    }
    let actual_retained = u64::try_from(index.estimated_size_bytes()).unwrap_or(u64::MAX);
    let declared_ceiling = header
        .metadata
        .retained_size_bytes
        .saturating_mul(2)
        .saturating_add(1024 * 1024);
    if actual_retained > declared_ceiling {
        return Ok(SearchIndexLoad::Corrupt(
            "decoded index exceeds declared retained-size bound".into(),
        ));
    }
    Ok(SearchIndexLoad::Loaded {
        index: Box::new(index),
        metadata: header.metadata,
    })
}

pub fn persist_search_index(
    path: &Path,
    source: &SearchIndexSource,
    index: &SearchIndex,
) -> io::Result<SearchIndexMetadata> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "index path has no parent"))?;
    fs::create_dir_all(parent)?;
    cleanup_stale_temporary_files(path);
    let (publication_lock, contended) = PublicationLock::acquire(path)?;
    if contended {
        if let SearchIndexInspection::Present(metadata) = inspect_search_index(path)? {
            if metadata.format_version == SEARCH_INDEX_FORMAT_VERSION
                && metadata.schema_fingerprint == search_schema_fingerprint()
                && &metadata.source == source
                && verify_payload_checksum(path)?
            {
                return Ok(metadata);
            }
        }
    }
    let temporary = temporary_path(path);
    let result = (|| {
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&temporary)?;
        let mut writer = BufWriter::new(file);
        let metadata = SearchIndexMetadata {
            format_version: SEARCH_INDEX_FORMAT_VERSION,
            schema_fingerprint: search_schema_fingerprint(),
            source: source.clone(),
            retained_size_bytes: u64::try_from(index.estimated_size_bytes()).unwrap_or(u64::MAX),
            payload_len: 0,
        };
        let patch_offset = write_header(&mut writer, &metadata, [0; 32])?;
        let (payload_len, checksum) = {
            let mut payload = PayloadWriter::new(&mut writer);
            encode_index(&mut payload, index)?;
            payload.finish()
        };
        writer.flush()?;
        writer.seek(SeekFrom::Start(patch_offset))?;
        writer.write_all(&payload_len.to_le_bytes())?;
        writer.write_all(&checksum)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        fs::rename(&temporary, path)?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(SearchIndexMetadata {
            payload_len,
            ..metadata
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    drop(publication_lock);
    result
}

struct PublicationLock {
    path: PathBuf,
    _file: File,
}

impl PublicationLock {
    fn acquire(destination: &Path) -> io::Result<(Self, bool)> {
        const WAIT_LIMIT: Duration = Duration::from_secs(30);
        const STALE_AFTER: Duration = Duration::from_secs(10 * 60);

        let path = lock_path(destination);
        let started = Instant::now();
        let mut contended = false;
        loop {
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok((Self { path, _file: file }, contended));
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let stale = fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age >= STALE_AFTER);
                    if stale {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    contended = true;
                    if started.elapsed() >= WAIT_LIMIT {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "timed out waiting for search sidecar publication lock",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn verify_payload_checksum(path: &Path) -> io::Result<bool> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let header = read_header(&mut reader)?;
    let payload_start = reader.stream_position()?;
    if payload_start
        .checked_add(header.metadata.payload_len)
        .is_none_or(|expected| expected != file_len)
    {
        return Ok(false);
    }
    let mut hasher = blake3::Hasher::new();
    let mut remaining = header.metadata.payload_len;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(0);
        let read = reader.read(&mut buffer[..wanted])?;
        if read == 0 {
            return Ok(false);
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(hasher.finalize().as_bytes() == &header.payload_checksum)
}

impl Drop for PublicationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

fn cleanup_stale_temporary_files(destination: &Path) {
    const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
    let Some(parent) = destination.parent() else {
        return;
    };
    let Some(stem) = destination.file_stem().and_then(|value| value.to_str()) else {
        return;
    };
    let prefix = format!("{stem}.tmp-");
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_AFTER);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

struct Header {
    metadata: SearchIndexMetadata,
    payload_checksum: [u8; 32],
}

fn write_header(
    writer: &mut (impl Write + Seek),
    metadata: &SearchIndexMetadata,
    checksum: [u8; 32],
) -> io::Result<u64> {
    writer.write_all(MAGIC)?;
    writer.write_all(&metadata.format_version.to_le_bytes())?;
    writer.write_all(&metadata.schema_fingerprint)?;
    write_string(writer, &metadata.source.artifact_version)?;
    writer.write_all(&metadata.source.nodes_len.to_le_bytes())?;
    writer.write_all(&metadata.source.nodes_modified_secs.to_le_bytes())?;
    writer.write_all(&metadata.source.nodes_modified_nanos.to_le_bytes())?;
    writer.write_all(&metadata.retained_size_bytes.to_le_bytes())?;
    let patch_offset = writer.stream_position()?;
    writer.write_all(&metadata.payload_len.to_le_bytes())?;
    writer.write_all(&checksum)?;
    Ok(patch_offset)
}

fn read_header(reader: &mut (impl Read + Seek)) -> io::Result<Header> {
    let mut magic = [0; 8];
    reader.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(invalid("invalid search index magic"));
    }
    let format_version = read_u32(reader)?;
    let mut schema_fingerprint = [0; 32];
    reader.read_exact(&mut schema_fingerprint)?;
    let artifact_version = read_string(reader, MAX_ARTIFACT_VERSION_BYTES)?;
    let nodes_len = read_u64(reader)?;
    let nodes_modified_secs = read_u64(reader)?;
    let nodes_modified_nanos = read_u32(reader)?;
    if nodes_modified_nanos >= 1_000_000_000 {
        return Err(invalid("invalid source mtime nanoseconds"));
    }
    let retained_size_bytes = read_u64(reader)?;
    let payload_len = read_u64(reader)?;
    if payload_len > MAX_PAYLOAD_BYTES {
        return Err(invalid("search index payload exceeds format limit"));
    }
    let mut payload_checksum = [0; 32];
    reader.read_exact(&mut payload_checksum)?;
    Ok(Header {
        metadata: SearchIndexMetadata {
            format_version,
            schema_fingerprint,
            source: SearchIndexSource {
                artifact_version,
                nodes_len,
                nodes_modified_secs,
                nodes_modified_nanos,
            },
            retained_size_bytes,
            payload_len,
        },
        payload_checksum,
    })
}

fn encode_index(writer: &mut impl Write, index: &SearchIndex) -> io::Result<()> {
    write_len(writer, index.docs.len())?;
    for doc in &index.docs {
        write_string(writer, doc.node_id.as_str())?;
        writer.write_all(&[kind_to_u8(doc.kind)])?;
        write_string(writer, &doc.name)?;
        match &doc.qualified_name {
            Some(value) => {
                writer.write_all(&[1])?;
                write_string(writer, value)?;
            }
            None => writer.write_all(&[0])?,
        }
        writer.write_all(&doc.file_id.to_le_bytes())?;
        writer.write_all(&doc.range.start_line.to_le_bytes())?;
        writer.write_all(&doc.range.start_col.to_le_bytes())?;
        writer.write_all(&doc.range.end_line.to_le_bytes())?;
        writer.write_all(&doc.range.end_col.to_le_bytes())?;
    }

    write_len(writer, index.files.len())?;
    for file in &index.files {
        write_string(writer, file)?;
    }
    writer.write_all(&index.avg_doc_len.to_bits().to_le_bytes())?;
    write_len(writer, index.doc_len.len())?;
    for &length in &index.doc_len {
        writer.write_all(&length.to_le_bytes())?;
    }

    let mut terms: Vec<_> = index.postings.iter().collect();
    terms.sort_unstable_by(|left, right| left.0.cmp(right.0));
    write_len(writer, terms.len())?;
    for (term, postings) in terms {
        write_string(writer, term)?;
        write_len(writer, postings.len())?;
        for &(doc_idx, frequency) in postings.iter() {
            writer.write_all(&doc_idx.to_le_bytes())?;
            writer.write_all(&frequency.to_le_bytes())?;
        }
    }
    Ok(())
}

fn decode_index(reader: &mut PayloadReader<impl Read>) -> io::Result<SearchIndex> {
    let docs_len = read_payload_len(reader, 30, "documents")?;
    let mut docs = Vec::with_capacity(docs_len);
    for _ in 0..docs_len {
        let node_id = NodeId::new(read_payload_string(reader, MAX_STRING_BYTES)?);
        let mut kind = [0];
        reader.read_exact(&mut kind)?;
        let kind = u8_to_kind(kind[0])?;
        let name = read_payload_string(reader, MAX_STRING_BYTES)?;
        let mut optional = [0];
        reader.read_exact(&mut optional)?;
        let qualified_name = match optional[0] {
            0 => None,
            1 => Some(read_payload_string(reader, MAX_STRING_BYTES)?),
            _ => return Err(invalid("invalid qualified-name marker")),
        };
        let file_id = read_u32(reader)?;
        let range = Range {
            start_line: read_u32(reader)?,
            start_col: read_u32(reader)?,
            end_line: read_u32(reader)?,
            end_col: read_u32(reader)?,
        };
        docs.push(IndexedDoc {
            node_id,
            kind,
            name,
            qualified_name,
            file_id,
            range,
        });
    }

    let files_len = read_payload_len(reader, 4, "files")?;
    let mut files = Vec::with_capacity(files_len);
    for _ in 0..files_len {
        files.push(read_payload_string(reader, MAX_STRING_BYTES)?);
    }
    if docs.iter().any(|doc| doc.file_id as usize >= files.len()) {
        return Err(invalid("document references an invalid file ordinal"));
    }

    let avg_doc_len = f32::from_bits(read_u32(reader)?);
    if !avg_doc_len.is_finite() || avg_doc_len < 0.0 {
        return Err(invalid("invalid average document length"));
    }
    let doc_len_count = read_payload_len(reader, 4, "document lengths")?;
    if doc_len_count != docs_len {
        return Err(invalid("document length vector does not match documents"));
    }
    let mut doc_len = Vec::with_capacity(doc_len_count);
    for _ in 0..doc_len_count {
        doc_len.push(read_u32(reader)?);
    }

    let term_count = read_payload_len(reader, 8, "terms")?;
    let mut postings = HashMap::with_capacity(term_count);
    let mut previous_term: Option<String> = None;
    for _ in 0..term_count {
        let term = read_payload_string(reader, MAX_STRING_BYTES)?;
        if previous_term
            .as_ref()
            .is_some_and(|previous| previous >= &term)
        {
            return Err(invalid("term table is not strictly sorted"));
        }
        previous_term = Some(term.clone());
        let posting_count = read_payload_len(reader, 8, "postings")?;
        let mut values = Vec::with_capacity(posting_count);
        let mut previous_doc = None;
        for _ in 0..posting_count {
            let doc_idx = read_u32(reader)?;
            let frequency = read_u32(reader)?;
            if doc_idx as usize >= docs_len || frequency == 0 {
                return Err(invalid("invalid posting"));
            }
            if previous_doc.is_some_and(|previous| previous >= doc_idx) {
                return Err(invalid("posting ordinals are not strictly increasing"));
            }
            previous_doc = Some(doc_idx);
            values.push((doc_idx, frequency));
        }
        postings.insert(term, values.into_boxed_slice());
    }

    Ok(SearchIndex {
        docs,
        files,
        avg_doc_len,
        postings,
        doc_len,
    })
}

struct PayloadWriter<W> {
    inner: W,
    hasher: blake3::Hasher,
    written: u64,
}

impl<W: Write> PayloadWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
            written: 0,
        }
    }

    fn finish(self) -> (u64, [u8; 32]) {
        (self.written, *self.hasher.finalize().as_bytes())
    }
}

impl<W: Write> Write for PayloadWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.hasher.update(&bytes[..written]);
        self.written = self.written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct PayloadReader<R> {
    inner: R,
    hasher: blake3::Hasher,
    remaining: u64,
}

impl<R: Read> PayloadReader<R> {
    fn new(inner: R, remaining: u64) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
            remaining,
        }
    }

    fn checksum(&self) -> [u8; 32] {
        *self.hasher.clone().finalize().as_bytes()
    }
}

impl<R: Read> Read for PayloadReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let allowed = usize::try_from(self.remaining.min(buffer.len() as u64)).unwrap_or(0);
        let read = self.inner.read(&mut buffer[..allowed])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated search index payload",
            ));
        }
        self.hasher.update(&buffer[..read]);
        self.remaining -= read as u64;
        Ok(read)
    }
}

fn write_len(writer: &mut impl Write, value: usize) -> io::Result<()> {
    let value = u32::try_from(value).map_err(|_| invalid("collection exceeds u32 format"))?;
    writer.write_all(&value.to_le_bytes())
}

fn read_len(reader: &mut impl Read) -> io::Result<usize> {
    let value = read_u32(reader)? as usize;
    if value > MAX_COLLECTION_ITEMS {
        return Err(invalid("collection exceeds search index limit"));
    }
    Ok(value)
}

fn read_payload_len(
    reader: &mut PayloadReader<impl Read>,
    minimum_item_bytes: u64,
    label: &'static str,
) -> io::Result<usize> {
    let count = read_len(reader)?;
    if u64::try_from(count)
        .unwrap_or(u64::MAX)
        .saturating_mul(minimum_item_bytes)
        > reader.remaining
    {
        return Err(invalid(format!(
            "declared {label} count exceeds remaining payload"
        )));
    }
    Ok(count)
}

fn write_string(writer: &mut impl Write, value: &str) -> io::Result<()> {
    write_len(writer, value.len())?;
    writer.write_all(value.as_bytes())
}

fn read_string(reader: &mut impl Read, max: usize) -> io::Result<String> {
    let len = read_len(reader)?;
    if len > max {
        return Err(invalid("string exceeds search index limit"));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|error| invalid(error.to_string()))
}

fn read_payload_string(reader: &mut PayloadReader<impl Read>, max: usize) -> io::Result<String> {
    let len = read_len(reader)?;
    if len > max || u64::try_from(len).unwrap_or(u64::MAX) > reader.remaining {
        return Err(invalid("string exceeds search index payload bounds"));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    String::from_utf8(bytes).map_err(|error| invalid(error.to_string()))
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn temporary_path(path: &Path) -> PathBuf {
    static NONCE: AtomicU64 = AtomicU64::new(1);
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    path.with_extension(format!(
        "tmp-{}-{clock}-{}",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn kind_to_u8(kind: NodeKind) -> u8 {
    match kind {
        NodeKind::File => 0,
        NodeKind::Folder => 1,
        NodeKind::Class => 2,
        NodeKind::Interface => 3,
        NodeKind::Enum => 4,
        NodeKind::Record => 5,
        NodeKind::Annotation => 6,
        NodeKind::Method => 7,
        NodeKind::Function => 8,
        NodeKind::Constructor => 9,
        NodeKind::Field => 10,
        NodeKind::Route => 11,
        NodeKind::Community => 12,
        NodeKind::Process => 13,
        NodeKind::KafkaTopic => 14,
        NodeKind::ExternalEndpoint => 15,
        NodeKind::DbQuery => 16,
        NodeKind::DbTable => 17,
        NodeKind::IntegrationRoute => 18,
        NodeKind::MessageDestination => 19,
        NodeKind::Other => 20,
    }
}

fn u8_to_kind(value: u8) -> io::Result<NodeKind> {
    match value {
        0 => Ok(NodeKind::File),
        1 => Ok(NodeKind::Folder),
        2 => Ok(NodeKind::Class),
        3 => Ok(NodeKind::Interface),
        4 => Ok(NodeKind::Enum),
        5 => Ok(NodeKind::Record),
        6 => Ok(NodeKind::Annotation),
        7 => Ok(NodeKind::Method),
        8 => Ok(NodeKind::Function),
        9 => Ok(NodeKind::Constructor),
        10 => Ok(NodeKind::Field),
        11 => Ok(NodeKind::Route),
        12 => Ok(NodeKind::Community),
        13 => Ok(NodeKind::Process),
        14 => Ok(NodeKind::KafkaTopic),
        15 => Ok(NodeKind::ExternalEndpoint),
        16 => Ok(NodeKind::DbQuery),
        17 => Ok(NodeKind::DbTable),
        18 => Ok(NodeKind::IntegrationRoute),
        19 => Ok(NodeKind::MessageDestination),
        20 => Ok(NodeKind::Other),
        _ => Err(invalid("unknown node kind ordinal")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bm25::{B, K1};
    use cih_core::Node;

    fn sample_index() -> SearchIndex {
        SearchIndex::build(&[Node {
            id: NodeId::new("Method:com.acme.OwnerService#findAll/0"),
            kind: NodeKind::Method,
            name: "findAll".into(),
            qualified_name: Some("com.acme.OwnerService.findAll".into()),
            file: "src/OwnerService.java".into(),
            range: Range {
                start_line: 5,
                start_col: 1,
                end_line: 9,
                end_col: 2,
            },
            props: None,
        }])
    }

    fn source(path: &Path, version: &str) -> SearchIndexSource {
        SearchIndexSource::from_nodes_file(path, version).unwrap()
    }

    #[test]
    fn round_trip_and_source_validation() {
        let directory = std::env::temp_dir().join(format!(
            "cih-search-persist-{}-{}",
            std::process::id(),
            temporary_path(Path::new("nonce"))
                .extension()
                .unwrap()
                .to_string_lossy()
        ));
        fs::create_dir_all(&directory).unwrap();
        let nodes = directory.join("nodes.jsonl");
        fs::write(&nodes, "node\n").unwrap();
        let source = source(&nodes, "v1");
        let path = search_index_path(&directory);
        let index = sample_index();
        let metadata = persist_search_index(&path, &source, &index).unwrap();
        assert!(metadata.payload_len > 0);
        assert_eq!(
            inspect_search_index(&path).unwrap(),
            SearchIndexInspection::Present(metadata.clone())
        );
        let loaded = load_search_index(&path, &source).unwrap();
        let SearchIndexLoad::Loaded { index: loaded, .. } = loaded else {
            panic!("expected loaded index");
        };
        assert_eq!(
            loaded.search("owner service", 10)[0].node_id.as_str(),
            "Method:com.acme.OwnerService#findAll/0"
        );

        let mut changed = source.clone();
        changed.nodes_len += 1;
        assert!(matches!(
            load_search_index(&path, &changed).unwrap(),
            SearchIndexLoad::Stale(_)
        ));
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn corruption_is_rejected() {
        let directory = std::env::temp_dir().join(format!(
            "cih-search-corrupt-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let nodes = directory.join("nodes.jsonl");
        fs::write(&nodes, "node\n").unwrap();
        let source = source(&nodes, "v1");
        let path = search_index_path(&directory);
        persist_search_index(&path, &source, &sample_index()).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            load_search_index(&path, &source).unwrap(),
            SearchIndexLoad::Corrupt(_)
        ));
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn contended_publication_rechecks_existing_destination() {
        let directory = std::env::temp_dir().join(format!(
            "cih-search-lock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let nodes = directory.join("nodes.jsonl");
        fs::write(&nodes, "node\n").unwrap();
        let source = source(&nodes, "v1");
        let path = search_index_path(&directory);
        let index = sample_index();
        persist_search_index(&path, &source, &index).unwrap();
        let original = fs::read(&path).unwrap();

        let lock = lock_path(&path);
        fs::write(&lock, b"other publisher").unwrap();
        let release = lock.clone();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(75));
            fs::remove_file(release).unwrap();
        });
        let metadata = persist_search_index(&path, &source, &index).unwrap();
        releaser.join().unwrap();

        assert_eq!(metadata.source, source);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!lock.exists());
        fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn bm25_constants_are_part_of_the_schema_contract() {
        assert_eq!(K1.to_bits(), 1.2_f32.to_bits());
        assert_eq!(B.to_bits(), 0.75_f32.to_bits());
        assert_ne!(search_schema_fingerprint(), [0; 32]);
    }
}
