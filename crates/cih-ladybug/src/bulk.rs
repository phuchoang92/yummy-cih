//! Bulk load: JSONL artifacts → CSV → `COPY`, with a Cypher `MERGE` fallback
//! for loads into an already-populated graph (multi-set `discover` loads the
//! community set after the analyze set).
//!
//! JSONL is streamed once per file; CSVs land in a temp dir next to the
//! version dir and are deleted after `COPY`. Edges whose endpoints are not in
//! the node set are skipped (the Falkor Cypher path drops them silently via
//! `MATCH`; `COPY` would abort on them).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use cih_core::{Edge, GraphArtifacts, Node};
use cih_graph_store::{GraphStoreError, LoadObserver, LoadStats, Result};
use lbug::Connection;

use crate::convert::{copt, cstr};
use crate::schema::{EDGE_COLUMNS, NODE_COLUMNS};

fn prop_str<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    node.props.as_ref()?.get(key)?.as_str()
}

fn prop_u64(node: &Node, key: &str) -> Option<u64> {
    node.props.as_ref()?.get(key)?.as_u64()
}

fn prop_f64(node: &Node, key: &str) -> Option<f64> {
    node.props.as_ref()?.get(key)?.as_f64()
}

fn opt_u64(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_default()
}

/// The 20 CSV cells for a node, in [`NODE_COLUMNS`] order (empty cell = NULL).
fn node_record(n: &Node) -> [String; 20] {
    [
        n.id.as_str().to_string(),
        n.name.clone(),
        n.kind.label().to_string(),
        n.file.clone(),
        n.qualified_name.clone().unwrap_or_default(),
        n.range.start_line.to_string(),
        n.range.end_line.to_string(),
        n.props.as_ref().map(|p| p.to_string()).unwrap_or_default(),
        prop_str(n, "stereotype").unwrap_or_default().to_string(),
        prop_str(n, "httpMethod").unwrap_or_default().to_string(),
        prop_str(n, "path").unwrap_or_default().to_string(),
        prop_str(n, "decorator").unwrap_or_default().to_string(),
        prop_str(n, "handler").unwrap_or_default().to_string(),
        opt_u64(prop_u64(n, "symbolCount").or_else(|| prop_u64(n, "symbol_count"))),
        prop_f64(n, "cohesion")
            .map(|f| f.to_string())
            .unwrap_or_default(),
        prop_str(n, "process_type").unwrap_or_default().to_string(),
        opt_u64(prop_u64(n, "cyclomatic")),
        opt_u64(prop_u64(n, "cognitive")),
        opt_u64(prop_u64(n, "loopDepth")),
        opt_u64(prop_u64(n, "transitiveLoopDepth")),
    ]
}

fn edge_call_sites(e: &Edge) -> Option<String> {
    e.props
        .as_ref()
        .and_then(|p| p.get("call_sites"))
        .map(|v| v.to_string())
}

/// True when the Symbol table is empty (routes between the COPY fast path and
/// the MERGE fallback, mirroring the Falkor adapter's `graph_is_empty`).
fn graph_is_empty(conn: &Connection) -> Result<bool> {
    let result = conn
        .query("MATCH (n:Symbol) RETURN count(n)")
        .map_err(|e| GraphStoreError::Backend(format!("count nodes: {e}")))?;
    for row in result {
        if let Some(v) = row.first() {
            return Ok(crate::convert::cell_u64(v) == 0);
        }
    }
    Ok(true)
}

pub(crate) fn load_observed(
    conn: &Connection,
    version_dir: &Path,
    artifacts: &GraphArtifacts,
    obs: &dyn LoadObserver,
) -> Result<LoadStats> {
    let stats = if graph_is_empty(conn)? {
        copy_load(conn, version_dir, artifacts, obs)?
    } else {
        merge_load(conn, artifacts, obs)?
    };
    conn.query("CHECKPOINT")
        .map_err(|e| GraphStoreError::Backend(format!("checkpoint after load: {e}")))?;
    obs.indexes_built();
    Ok(stats)
}

/// Fresh-graph fast path: stream JSONL → CSVs → one COPY per table.
fn copy_load(
    conn: &Connection,
    version_dir: &Path,
    artifacts: &GraphArtifacts,
    obs: &dyn LoadObserver,
) -> Result<LoadStats> {
    let csv_dir = version_dir.with_extension("csv-tmp");
    std::fs::create_dir_all(&csv_dir)
        .map_err(|e| GraphStoreError::Backend(format!("create csv tmp dir: {e}")))?;
    let cleanup = TempDir(&csv_dir);

    let io_err = |what: &str, e: std::io::Error| GraphStoreError::Backend(format!("{what}: {e}"));

    // Node pass: write nodes.csv, dedupe ids (PK violations abort COPY).
    let nodes_csv = csv_dir.join("nodes.csv");
    let mut ids = HashSet::<String>::new();
    {
        let mut w = csv::Writer::from_path(&nodes_csv)
            .map_err(|e| GraphStoreError::Backend(format!("open nodes.csv: {e}")))?;
        w.write_record(NODE_COLUMNS)
            .map_err(|e| GraphStoreError::Backend(format!("nodes.csv header: {e}")))?;
        let stream = artifacts
            .stream_nodes()
            .map_err(|e| io_err("read nodes.jsonl", e))?;
        for node in stream {
            let node = node.map_err(|e| io_err("read nodes.jsonl", e))?;
            if !ids.insert(node.id.as_str().to_string()) {
                continue;
            }
            w.write_record(node_record(&node))
                .map_err(|e| GraphStoreError::Backend(format!("write nodes.csv: {e}")))?;
        }
        w.flush()
            .map_err(|e| GraphStoreError::Backend(format!("flush nodes.csv: {e}")))?;
    }
    let total_nodes = ids.len() as u64;
    if total_nodes == 0 {
        return Ok(LoadStats { nodes: 0, edges: 0 });
    }

    // Edge pass: partition into one CSV per kind; skip dangling endpoints and
    // duplicates. Kùzu rel tables are multigraphs, so without the dedup a
    // repeated artifact edge would double-count here while the Falkor
    // GRAPH.BULK path (cih-falkor/src/bulk.rs) and our own MERGE fallback
    // both collapse it — parity requires the same (src, dst, kind) identity.
    let mut writers: HashMap<&'static str, csv::Writer<std::fs::File>> = HashMap::new();
    let mut seen_edges: HashSet<(String, String, cih_core::EdgeKind)> = HashSet::new();
    let mut total_edges = 0u64;
    let stream = artifacts
        .stream_edges()
        .map_err(|e| io_err("read edges.jsonl", e))?;
    for edge in stream {
        let edge = edge.map_err(|e| io_err("read edges.jsonl", e))?;
        if !ids.contains(edge.src.as_str()) || !ids.contains(edge.dst.as_str()) {
            continue;
        }
        if !seen_edges.insert((
            edge.src.as_str().to_string(),
            edge.dst.as_str().to_string(),
            edge.kind,
        )) {
            continue;
        }
        let label = edge.kind.cypher_label();
        let w = match writers.entry(label) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(slot) => {
                let mut w = csv::Writer::from_path(csv_dir.join(format!("{label}.csv")))
                    .map_err(|e| GraphStoreError::Backend(format!("open {label}.csv: {e}")))?;
                w.write_record(EDGE_COLUMNS)
                    .map_err(|e| GraphStoreError::Backend(format!("{label}.csv header: {e}")))?;
                slot.insert(w)
            }
        };
        w.write_record([
            edge.src.as_str().to_string(),
            edge.dst.as_str().to_string(),
            edge.confidence.to_string(),
            edge.reason.clone(),
            edge_call_sites(&edge).unwrap_or_default(),
        ])
        .map_err(|e| GraphStoreError::Backend(format!("write {label}.csv: {e}")))?;
        total_edges += 1;
    }
    let kinds: Vec<&'static str> = writers.keys().copied().collect();
    for (_, mut w) in writers.drain() {
        w.flush()
            .map_err(|e| GraphStoreError::Backend(format!("flush edge csv: {e}")))?;
    }

    // COPY node table, then each rel table present in the artifacts.
    // parallel=false: the parallel CSV reader rejects quoted newlines, which
    // real node names/props can contain (contract-tested).
    let copy = |table: &str, file: &Path| -> Result<()> {
        let q = format!(
            "COPY {table} FROM {} (header=true, parallel=false)",
            cstr(&file.to_string_lossy())
        );
        conn.query(&q)
            .map_err(|e| GraphStoreError::Backend(format!("COPY {table}: {e}")))?;
        Ok(())
    };
    copy("Symbol", &nodes_csv)?;
    obs.nodes_loaded(total_nodes);
    for label in kinds {
        copy(label, &csv_dir.join(format!("{label}.csv")))?;
    }
    obs.edges_loaded(total_edges);

    drop(cleanup);
    Ok(LoadStats {
        nodes: total_nodes,
        edges: total_edges,
    })
}

/// Populated-graph fallback: per-row `MERGE` statements (used for the small
/// community set and for `upsert_incremental` deltas).
pub(crate) fn merge_load(
    conn: &Connection,
    artifacts: &GraphArtifacts,
    obs: &dyn LoadObserver,
) -> Result<LoadStats> {
    let nodes = artifacts
        .read_nodes()
        .map_err(|e| GraphStoreError::Backend(format!("read nodes: {e}")))?;
    let edges = artifacts
        .read_edges()
        .map_err(|e| GraphStoreError::Backend(format!("read edges: {e}")))?;
    merge_nodes_edges(conn, &nodes, &edges, obs)
}

pub(crate) fn merge_nodes_edges(
    conn: &Connection,
    nodes: &[Node],
    edges: &[Edge],
    obs: &dyn LoadObserver,
) -> Result<LoadStats> {
    for n in nodes {
        let rec = node_record(n);
        // MERGE by PK, then SET every column (delta rows must overwrite).
        let sets: Vec<String> = NODE_COLUMNS
            .iter()
            .zip(rec.iter())
            .skip(1) // id is the MERGE key
            .map(|(col, val)| {
                let lit = match *col {
                    "sl"
                    | "el"
                    | "symbolCount"
                    | "cyclomatic"
                    | "cognitive"
                    | "loopDepth"
                    | "transitiveLoopDepth"
                    | "cohesion" => {
                        if val.is_empty() {
                            "NULL".to_string()
                        } else {
                            val.clone()
                        }
                    }
                    _ => copt((!val.is_empty()).then_some(val.as_str())),
                };
                format!("n.{col} = {lit}")
            })
            .collect();
        let q = format!(
            "MERGE (n:Symbol {{id: {id}}}) SET {sets}",
            id = cstr(n.id.as_str()),
            sets = sets.join(", ")
        );
        conn.query(&q)
            .map_err(|e| GraphStoreError::Backend(format!("merge node: {e}")))?;
    }
    obs.nodes_loaded(nodes.len() as u64);

    let mut loaded_edges = 0u64;
    for e in edges {
        let label = e.kind.cypher_label();
        let q = format!(
            "MATCH (a:Symbol {{id: {src}}}), (b:Symbol {{id: {dst}}}) \
             MERGE (a)-[r:{label}]->(b) \
             SET r.confidence = {conf}, r.reason = {reason}, r.callSites = {cs}",
            src = cstr(e.src.as_str()),
            dst = cstr(e.dst.as_str()),
            conf = e.confidence,
            reason = cstr(&e.reason),
            cs = copt(edge_call_sites(e).as_deref()),
        );
        conn.query(&q)
            .map_err(|err| GraphStoreError::Backend(format!("merge {label} edge: {err}")))?;
        loaded_edges += 1;
    }
    obs.edges_loaded(loaded_edges);
    Ok(LoadStats {
        nodes: nodes.len() as u64,
        edges: loaded_edges,
    })
}

/// RAII cleanup for the CSV temp dir.
struct TempDir<'a>(&'a PathBuf);
impl Drop for TempDir<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0);
    }
}
