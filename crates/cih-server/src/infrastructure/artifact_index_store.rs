//! Versioned binary sidecar for artifact indexes.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::ports::artifact_index_store::{ArtifactIndexStore, ArtifactSourceIdentity};
use crate::ports::artifact_repository::ArtifactIndexes;

const MAGIC: &[u8; 8] = b"CIHIDX01";
const FORMAT_VERSION: u32 = 1;
const FILE_NAME: &str = "cih-server-index-v1.bin";
const MAX_COLLECTION_ITEMS: usize = 100_000_000;

#[derive(Clone, Default)]
pub(crate) struct FileArtifactIndexStore;

impl ArtifactIndexStore for FileArtifactIndexStore {
    fn load(
        &self,
        artifacts_dir: &Path,
        source: ArtifactSourceIdentity,
    ) -> io::Result<Option<ArtifactIndexes>> {
        let path = artifacts_dir.join(FILE_NAME);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        match decode(&bytes, source) {
            Ok(indexes) => Ok(Some(indexes)),
            Err(error) => {
                tracing::warn!(path = %path.display(), error = %error, "ignoring invalid artifact index sidecar");
                Ok(None)
            }
        }
    }

    fn persist(
        &self,
        artifacts_dir: &Path,
        source: ArtifactSourceIdentity,
        indexes: &ArtifactIndexes,
    ) -> io::Result<()> {
        let path = artifacts_dir.join(FILE_NAME);
        let temporary = temporary_path(&path);
        let bytes = encode(source, indexes)?;
        let result = (|| {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, &path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension(format!("tmp-{}", std::process::id()))
}

fn encode(source: ArtifactSourceIdentity, indexes: &ArtifactIndexes) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    put_u32(&mut bytes, FORMAT_VERSION);
    put_source(&mut bytes, source);
    put_node_map(&mut bytes, &indexes.node_by_id)?;
    put_adjacency(&mut bytes, &indexes.outgoing_edges)?;
    put_adjacency(&mut bytes, &indexes.incoming_edges)?;
    let checksum = checksum(&bytes);
    put_u64(&mut bytes, checksum);
    Ok(bytes)
}

fn decode(bytes: &[u8], expected: ArtifactSourceIdentity) -> io::Result<ArtifactIndexes> {
    if bytes.len() < MAGIC.len() + 4 + 8 {
        return Err(invalid("index is too short"));
    }
    let (payload, checksum_bytes) = bytes.split_at(bytes.len() - 8);
    let stored_checksum = u64::from_le_bytes(checksum_bytes.try_into().unwrap());
    if checksum(payload) != stored_checksum {
        return Err(invalid("checksum mismatch"));
    }
    let mut cursor = Cursor::new(payload);
    if cursor.take(MAGIC.len())? != MAGIC {
        return Err(invalid("magic mismatch"));
    }
    if cursor.u32()? != FORMAT_VERSION {
        return Err(invalid("unsupported format version"));
    }
    if cursor.source()? != expected {
        return Err(invalid("source artifact identity changed"));
    }
    let node_by_id = cursor.node_map()?;
    let outgoing_edges = cursor.adjacency()?;
    let incoming_edges = cursor.adjacency()?;
    if !cursor.remaining().is_empty() {
        return Err(invalid("trailing payload bytes"));
    }
    Ok(ArtifactIndexes {
        node_by_id,
        outgoing_edges,
        incoming_edges,
    })
}

fn put_source(bytes: &mut Vec<u8>, source: ArtifactSourceIdentity) {
    for identity in [source.nodes, source.edges] {
        put_u64(bytes, identity.len);
        put_u64(bytes, identity.modified_secs);
        put_u32(bytes, identity.modified_nanos);
    }
}

fn put_node_map(bytes: &mut Vec<u8>, values: &HashMap<String, usize>) -> io::Result<()> {
    put_len(bytes, values.len())?;
    let mut entries: Vec<_> = values.iter().collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    for (key, value) in entries {
        put_string(bytes, key)?;
        put_u64(bytes, *value as u64);
    }
    Ok(())
}

fn put_adjacency(bytes: &mut Vec<u8>, values: &HashMap<String, Vec<usize>>) -> io::Result<()> {
    put_len(bytes, values.len())?;
    let mut entries: Vec<_> = values.iter().collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    for (key, positions) in entries {
        put_string(bytes, key)?;
        put_len(bytes, positions.len())?;
        for position in positions {
            put_u64(bytes, *position as u64);
        }
    }
    Ok(())
}

fn put_string(bytes: &mut Vec<u8>, value: &str) -> io::Result<()> {
    put_len(bytes, value.len())?;
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

fn put_len(bytes: &mut Vec<u8>, value: usize) -> io::Result<()> {
    let value = u32::try_from(value).map_err(|_| invalid("collection exceeds u32 format limit"))?;
    put_u32(bytes, value);
    Ok(())
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> io::Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| invalid("unexpected end of index"))?;
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn len(&mut self) -> io::Result<usize> {
        let len = self.u32()? as usize;
        if len > MAX_COLLECTION_ITEMS {
            return Err(invalid("collection count exceeds safety limit"));
        }
        Ok(len)
    }

    fn string(&mut self) -> io::Result<String> {
        let len = self.len()?;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| invalid("index key is not UTF-8"))
    }

    fn source(&mut self) -> io::Result<ArtifactSourceIdentity> {
        let mut identity =
            || -> io::Result<crate::ports::artifact_index_store::SourceFileIdentity> {
                Ok(crate::ports::artifact_index_store::SourceFileIdentity {
                    len: self.u64()?,
                    modified_secs: self.u64()?,
                    modified_nanos: self.u32()?,
                })
            };
        Ok(ArtifactSourceIdentity {
            nodes: identity()?,
            edges: identity()?,
        })
    }

    fn node_map(&mut self) -> io::Result<HashMap<String, usize>> {
        let len = self.len()?;
        let mut values = HashMap::with_capacity(len);
        for _ in 0..len {
            let key = self.string()?;
            let value = usize::try_from(self.u64()?).map_err(|_| invalid("ordinal overflow"))?;
            values.insert(key, value);
        }
        Ok(values)
    }

    fn adjacency(&mut self) -> io::Result<HashMap<String, Vec<usize>>> {
        let len = self.len()?;
        let mut values = HashMap::with_capacity(len);
        for _ in 0..len {
            let key = self.string()?;
            let positions_len = self.len()?;
            let mut positions = Vec::with_capacity(positions_len);
            for _ in 0..positions_len {
                positions.push(
                    usize::try_from(self.u64()?).map_err(|_| invalid("edge ordinal overflow"))?,
                );
            }
            values.insert(key, positions);
        }
        Ok(values)
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.offset..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::artifact_index_store::SourceFileIdentity;

    fn source(len: u64) -> ArtifactSourceIdentity {
        let identity = SourceFileIdentity {
            len,
            modified_secs: 10,
            modified_nanos: 20,
        };
        ArtifactSourceIdentity {
            nodes: identity,
            edges: identity,
        }
    }

    fn indexes() -> ArtifactIndexes {
        ArtifactIndexes {
            node_by_id: HashMap::from([("node-a".into(), 2)]),
            outgoing_edges: HashMap::from([("node-a".into(), vec![1, 4])]),
            incoming_edges: HashMap::from([("node-b".into(), vec![1])]),
        }
    }

    #[test]
    fn round_trip_rejects_changed_source_and_checksum() {
        let encoded = encode(source(1), &indexes()).unwrap();
        let decoded = decode(&encoded, source(1)).unwrap();
        assert_eq!(decoded.node_by_id["node-a"], 2);
        assert_eq!(decoded.outgoing_edges["node-a"], vec![1, 4]);
        assert!(decode(&encoded, source(2)).is_err());

        let mut corrupt = encoded;
        corrupt[20] ^= 0xff;
        assert!(decode(&corrupt, source(1)).is_err());
    }

    #[test]
    fn file_store_publishes_atomically_and_ignores_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let store = FileArtifactIndexStore;
        store
            .persist(directory.path(), source(1), &indexes())
            .unwrap();
        assert!(store.load(directory.path(), source(1)).unwrap().is_some());
        assert!(!temporary_path(&directory.path().join(FILE_NAME)).exists());

        fs::write(directory.path().join(FILE_NAME), b"partial").unwrap();
        assert!(store.load(directory.path(), source(1)).unwrap().is_none());
    }
}
