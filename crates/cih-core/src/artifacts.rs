//! Canonical bulk-load artifacts (JSONL v1).
//!
//! The engine emits `nodes.jsonl` + `edges.jsonl`; each `BulkLoader` reads these
//! and transforms them into its backend's load format. JSONL keeps Phase 2
//! dependency-free (serde_json only); swap to Parquet when the Neptune S3 loader
//! path needs columnar input (Phase 11).

use crate::{Edge, GraphArtifacts, Node, VersionId};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
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
}

fn write_jsonl<T: Serialize>(path: &Path, items: &[T]) -> std::io::Result<()> {
    let mut w = BufWriter::new(File::create(path)?);
    for it in items {
        let line = serde_json::to_string(it).map_err(io_err)?;
        w.write_all(line.as_bytes())?;
        w.write_all(b"\n")?;
    }
    w.flush()
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
