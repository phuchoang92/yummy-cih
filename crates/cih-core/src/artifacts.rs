//! Canonical bulk-load artifacts (JSONL v1) and bundle archive (Gap 5).
//!
//! The engine emits `nodes.jsonl` + `edges.jsonl`; each `BulkLoader` reads these
//! and transforms them into its backend's load format. JSONL keeps Phase 2
//! dependency-free (serde_json only); swap to Parquet when the Neptune S3 loader
//! path needs columnar input (Phase 11).
//!
//! Bundle format (Gap 5): `CIHPACK1` magic + entries, each entry = 4-byte LE
//! length + zstd-compressed blob.

use crate::{CihBundleManifest, Edge, GraphArtifacts, Node, VersionId};
use anyhow::{anyhow, Context as _};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

impl GraphArtifacts {
    /// Write `nodes.jsonl` + `edges.jsonl` into `dir` and return the handle.
    pub fn write(
        dir: &Path,
        version: VersionId,
        nodes: &[Node],
        edges: &[Edge],
    ) -> std::io::Result<GraphArtifacts> {
        fs::create_dir_all(dir)?;
        let nodes_path = dir.join("nodes.jsonl");
        let edges_path = dir.join("edges.jsonl");
        write_jsonl(&nodes_path, nodes)?;
        write_jsonl(&edges_path, edges)?;
        Ok(GraphArtifacts {
            nodes_path,
            edges_path,
            version,
        })
    }

    pub fn read_nodes(&self) -> std::io::Result<Vec<Node>> {
        read_jsonl(&self.nodes_path)
    }

    pub fn read_edges(&self) -> std::io::Result<Vec<Edge>> {
        read_jsonl(&self.edges_path)
    }

    /// Return the most-recent `GraphArtifacts` found directly under `parent`.
    ///
    /// Walks `parent`, keeps every immediate subdirectory that contains both
    /// `nodes.jsonl` and `edges.jsonl`, and returns the one with the newest
    /// `nodes.jsonl` mtime (ties broken by version string, descending).
    pub fn latest_in_dir(parent: &Path) -> anyhow::Result<GraphArtifacts> {
        let entries = std::fs::read_dir(parent)
            .with_context(|| format!("no artifacts at {}", parent.display()))?;
        let mut candidates: Vec<(std::time::SystemTime, GraphArtifacts)> = Vec::new();
        for entry in entries {
            let entry = entry?;
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let nodes_path = dir.join("nodes.jsonl");
            let edges_path = dir.join("edges.jsonl");
            if !nodes_path.is_file() || !edges_path.is_file() {
                continue;
            }
            let version = entry.file_name().to_string_lossy().into_owned();
            let modified = std::fs::metadata(&nodes_path)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            candidates.push((
                modified,
                GraphArtifacts {
                    nodes_path,
                    edges_path,
                    version: VersionId::new(version),
                },
            ));
        }
        candidates.sort_by(|(a_mtime, a_art), (b_mtime, b_art)| {
            b_mtime
                .cmp(a_mtime)
                .then_with(|| b_art.version.as_str().cmp(a_art.version.as_str()))
        });
        candidates
            .into_iter()
            .next()
            .map(|(_, a)| a)
            .ok_or_else(|| anyhow!("no complete artifacts under {}", parent.display()))
    }
}

fn write_jsonl<T: Serialize + Sync>(path: &Path, items: &[T]) -> std::io::Result<()> {
    let body = serialize_jsonl(items)?;
    let mut w = BufWriter::new(File::create(path)?);
    w.write_all(&body)?;
    w.flush()
}

/// Serialize `items` to newline-delimited JSON, one line per item.
///
/// Serialization is the CPU cost (not the write), so it runs in parallel over
/// rayon chunks; each chunk builds its own buffer and the chunks are then
/// concatenated in order — keeping output byte-identical to a sequential
/// `to_string` loop. Worth it for flat records like `Node`/`Edge`; not used
/// for the parse IR (large nested structs where materialization outweighs the
/// parallel win — measured on the fineract fixture).
fn serialize_jsonl<T: Serialize + Sync>(items: &[T]) -> std::io::Result<Vec<u8>> {
    use rayon::prelude::*;

    // Chunk so each rayon task amortizes buffer allocation over many items;
    // 2048 keeps chunk buffers cache-friendly while cutting task overhead.
    const CHUNK: usize = 2048;
    let chunks: Vec<Vec<u8>> = items
        .par_chunks(CHUNK)
        .map(|chunk| {
            let mut buf = Vec::with_capacity(chunk.len() * 256);
            for it in chunk {
                serde_json::to_writer(&mut buf, it).map_err(io_err)?;
                buf.push(b'\n');
            }
            Ok::<Vec<u8>, std::io::Error>(buf)
        })
        .collect::<std::io::Result<Vec<_>>>()?;

    let total = chunks.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for c in chunks {
        out.extend_from_slice(&c);
    }
    Ok(out)
}

fn read_jsonl<T: DeserializeOwned>(path: &Path) -> std::io::Result<Vec<T>> {
    let r = BufReader::new(File::open(path)?);
    let mut out = Vec::new();
    for line in r.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(&line).map_err(io_err)?);
    }
    Ok(out)
}

fn io_err(e: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
}

// ── Gap 5: Bundle archive ──────────────────────────────────────────────────

const BUNDLE_MAGIC: &[u8; 8] = b"CIHPACK1";

/// Write one entry to a bundle: 4-byte LE length + zstd-compressed content.
fn write_bundle_entry(w: &mut impl Write, content: &[u8]) -> io::Result<()> {
    let compressed = zstd::encode_all(content, 3).map_err(io::Error::other)?;
    // The entry length prefix is 4 bytes — refuse rather than silently truncate a
    // compressed entry that does not fit in u32 (> 4 GiB).
    let len = u32::try_from(compressed.len()).map_err(|_| {
        io::Error::other("bundle entry exceeds 4 GiB compressed; cannot encode length")
    })?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&compressed)?;
    Ok(())
}

/// Read one entry from a bundle: 4-byte LE length + zstd-compressed content.
fn read_bundle_entry(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut compressed = vec![0u8; len];
    r.read_exact(&mut compressed)?;
    zstd::decode_all(compressed.as_slice())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Read a file as bytes, returning empty vec if the file doesn't exist.
fn read_file_opt(path: &Path) -> io::Result<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

impl GraphArtifacts {
    /// Export a bundle archive to `dest`.
    ///
    /// Bundle entries (in order):
    /// 1. `manifest.json`
    /// 2. `nodes.jsonl`
    /// 3. `edges.jsonl`
    /// 4. `community-nodes.jsonl` (if `community` provided)
    /// 5. `community-edges.jsonl` (if `community` provided)
    /// 6. `file-hashes.json`
    /// 7. `scope.json`
    /// 8. `repo-map.json`
    pub fn export_bundle(
        &self,
        community: Option<&GraphArtifacts>,
        file_hashes: &Path,
        scope_json: &Path,
        repo_map_json: &Path,
        dest: &Path,
    ) -> io::Result<CihBundleManifest> {
        let has_community = community.is_some();

        // Build manifest.
        let repo_name = self
            .nodes_path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent()) // .cih/artifacts/<version>/
            .and_then(|p| p.parent()) // .cih/
            .and_then(|p| p.parent()) // repo_root
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();
        let root_path = self
            .nodes_path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let nodes = fs::read(&self.nodes_path)?;
        let edges = fs::read(&self.edges_path)?;

        // Count files from nodes.
        let file_count = nodes
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .filter(|l| {
                l.iter()
                    .position(|&b| b == b':')
                    .map(|i| l.get(i + 1..i + 7) == Some(b"\"File\""))
                    .unwrap_or(false)
            })
            .count();

        let manifest = CihBundleManifest {
            bundle_version: 1,
            cih_version: env!("CARGO_PKG_VERSION").to_string(),
            repo_name,
            root_path,
            indexed_at: crate::registry::now_rfc3339(),
            artifact_version: self.version.to_string(),
            has_community,
            file_count,
        };

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut w = BufWriter::new(File::create(dest)?);
        w.write_all(BUNDLE_MAGIC)?;

        let manifest_json = serde_json::to_vec(&manifest)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_bundle_entry(&mut w, &manifest_json)?;
        write_bundle_entry(&mut w, &nodes)?;
        write_bundle_entry(&mut w, &edges)?;

        if let Some(comm) = community {
            let cn = fs::read(&comm.nodes_path)?;
            let ce = fs::read(&comm.edges_path)?;
            write_bundle_entry(&mut w, &cn)?;
            write_bundle_entry(&mut w, &ce)?;
        } else {
            // Empty placeholders.
            write_bundle_entry(&mut w, b"")?;
            write_bundle_entry(&mut w, b"")?;
        }

        write_bundle_entry(&mut w, &read_file_opt(file_hashes)?)?;
        write_bundle_entry(&mut w, &read_file_opt(scope_json)?)?;
        write_bundle_entry(&mut w, &read_file_opt(repo_map_json)?)?;

        w.flush()?;
        Ok(manifest)
    }

    /// Import a bundle archive, restoring all files into `cih_dir`.
    ///
    /// Returns `(main_artifacts, community_artifacts_opt, manifest)`.
    pub fn import_bundle(
        bundle: &Path,
        cih_dir: &Path,
    ) -> io::Result<(GraphArtifacts, Option<GraphArtifacts>, CihBundleManifest)> {
        let mut r = File::open(bundle)?;
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != BUNDLE_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a CIH bundle (bad magic)",
            ));
        }

        let manifest_bytes = read_bundle_entry(&mut r)?;
        let manifest: CihBundleManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let nodes_bytes = read_bundle_entry(&mut r)?;
        let edges_bytes = read_bundle_entry(&mut r)?;
        let comm_nodes_bytes = read_bundle_entry(&mut r)?;
        let comm_edges_bytes = read_bundle_entry(&mut r)?;
        let file_hashes_bytes = read_bundle_entry(&mut r)?;
        let scope_bytes = read_bundle_entry(&mut r)?;
        let repo_map_bytes = read_bundle_entry(&mut r)?;

        // Restore into cih_dir.
        let art_dir = cih_dir.join("artifacts").join(&manifest.artifact_version);
        fs::create_dir_all(&art_dir)?;

        let nodes_path = art_dir.join("nodes.jsonl");
        let edges_path = art_dir.join("edges.jsonl");
        fs::write(&nodes_path, &nodes_bytes)?;
        fs::write(&edges_path, &edges_bytes)?;

        let main_artifacts = GraphArtifacts {
            nodes_path,
            edges_path,
            version: VersionId::new(manifest.artifact_version.clone()),
        };

        let community_artifacts = if manifest.has_community && !comm_nodes_bytes.is_empty() {
            let comm_dir = cih_dir
                .join("artifacts-community")
                .join(&manifest.artifact_version);
            fs::create_dir_all(&comm_dir)?;
            let cn = comm_dir.join("nodes.jsonl");
            let ce = comm_dir.join("edges.jsonl");
            fs::write(&cn, &comm_nodes_bytes)?;
            fs::write(&ce, &comm_edges_bytes)?;
            Some(GraphArtifacts {
                nodes_path: cn,
                edges_path: ce,
                version: VersionId::new(manifest.artifact_version.clone()),
            })
        } else {
            None
        };

        if !file_hashes_bytes.is_empty() {
            fs::write(cih_dir.join("file-hashes.json"), &file_hashes_bytes)?;
        }
        if !scope_bytes.is_empty() {
            fs::write(cih_dir.join("scope.json"), &scope_bytes)?;
        }
        if !repo_map_bytes.is_empty() {
            fs::write(cih_dir.join("repo-map.json"), &repo_map_bytes)?;
        }

        Ok((main_artifacts, community_artifacts, manifest))
    }
}
