//! Native FalkorDB `GRAPH.BULK` binary encoder.
//!
//! Bypasses the Cypher parser and per-edge `MATCH`: nodes are inserted by
//! ordinal (0-based insertion order) and edges reference their endpoints by that
//! 8-byte ordinal. Wire format (FalkorDB v4.18, matching `falkordb-bulk-loader`):
//!
//! - Command: `GRAPH.BULK <key> BEGIN <node_count> <edge_count>
//!   <label_blob_count> <reltype_blob_count> <node_blobs…> <reltype_blobs…>`.
//! - Blob header: `name\0`, `prop_count` (u32 LE), each `prop_name\0`.
//! - Value: 1 type-tag byte + payload — `NULL=0`, `DOUBLE=2` (f64 LE),
//!   `STRING=3` (utf8 + `\0`), `LONG=4` (i64 LE). Node row = the values in header
//!   order; edge row = `src_ord` (u64 LE) + `dst_ord` (u64 LE) + values.
//!
//! Parity with the Cypher load (`nodes_to_list`/`edges_to_list`) is exact:
//! duplicate node ids and duplicate `(src,dst,kind)` edges are collapsed
//! first-wins, and edges with an endpoint absent from the node set are dropped
//! (the Cypher `MATCH` drops them too).

use std::collections::{HashMap, HashSet};
use std::io;

use cih_core::{Edge, EdgeKind, Node};

use super::{prop_f64, prop_str, prop_u64};

const T_NULL: u8 = 0;
const T_DOUBLE: u8 = 2;
const T_STRING: u8 = 3;
const T_LONG: u8 = 4;

/// The fixed `:Symbol` property schema — mirrors `nodes_to_list` exactly.
const NODE_PROPS: [&str; 20] = [
    "id",
    "name",
    "kind",
    "file",
    "qualifiedName",
    "startLine",
    "endLine",
    "props",
    "stereotype",
    "httpMethod",
    "path",
    "decorator",
    "handler",
    "symbolCount",
    "cohesion",
    "processType",
    "cyclomatic",
    "cognitive",
    "loopDepth",
    "transitiveLoopDepth",
];

const EDGE_PROPS: [&str; 3] = ["confidence", "reason", "callSites"];

/// One `GRAPH.BULK` blob and the number of entities it holds.
pub(crate) type Batch = (u64, Vec<u8>);
/// Node id → 0-based ordinal. Owned keys so nodes can be dropped while streaming.
pub(crate) type OrdinalMap = HashMap<String, u64>;

// ── value writers ────────────────────────────────────────────────────────────

fn push_cstr(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(s.as_bytes());
    buf.push(0);
}

fn push_string(buf: &mut Vec<u8>, s: &str) {
    buf.push(T_STRING);
    push_cstr(buf, s);
}

fn push_long(buf: &mut Vec<u8>, v: i64) {
    buf.push(T_LONG);
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_double(buf: &mut Vec<u8>, v: f64) {
    buf.push(T_DOUBLE);
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_opt_string(buf: &mut Vec<u8>, v: Option<&str>) {
    match v {
        Some(s) => push_string(buf, s),
        None => buf.push(T_NULL),
    }
}

fn push_opt_long(buf: &mut Vec<u8>, v: Option<u64>) {
    match v {
        Some(n) => push_long(buf, n as i64),
        None => buf.push(T_NULL),
    }
}

fn push_opt_double(buf: &mut Vec<u8>, v: Option<f64>) {
    match v {
        Some(n) => push_double(buf, n),
        None => buf.push(T_NULL),
    }
}

fn write_header(buf: &mut Vec<u8>, name: &str, props: &[&str]) {
    push_cstr(buf, name);
    buf.extend_from_slice(&(props.len() as u32).to_le_bytes());
    for p in props {
        push_cstr(buf, p);
    }
}

/// Encode one node row: the 20 schema values in header order. Value extraction
/// matches `nodes_to_list` (note `processType` is read from the `process_type`
/// prop key, and `symbolCount` falls back to `symbol_count`).
fn encode_node(buf: &mut Vec<u8>, n: &Node) {
    push_string(buf, n.id.as_str());
    push_string(buf, &n.name);
    push_string(buf, n.kind.label());
    push_string(buf, &n.file);
    push_opt_string(buf, n.qualified_name.as_deref());
    push_long(buf, n.range.start_line as i64);
    push_long(buf, n.range.end_line as i64);
    let props_json = n.props.as_ref().map(|v| v.to_string());
    push_opt_string(buf, props_json.as_deref());
    push_opt_string(buf, prop_str(n, "stereotype"));
    push_opt_string(buf, prop_str(n, "httpMethod"));
    push_opt_string(buf, prop_str(n, "path"));
    push_opt_string(buf, prop_str(n, "decorator"));
    push_opt_string(buf, prop_str(n, "handler"));
    push_opt_long(
        buf,
        prop_u64(n, "symbolCount").or_else(|| prop_u64(n, "symbol_count")),
    );
    push_opt_double(buf, prop_f64(n, "cohesion"));
    push_opt_string(buf, prop_str(n, "process_type"));
    push_opt_long(buf, prop_u64(n, "cyclomatic"));
    push_opt_long(buf, prop_u64(n, "cognitive"));
    push_opt_long(buf, prop_u64(n, "loopDepth"));
    push_opt_long(buf, prop_u64(n, "transitiveLoopDepth"));
}

// ── payload assembly ─────────────────────────────────────────────────────────

/// Default per-`GRAPH.BULK`-call payload budget: 128 MiB, well under the 512 MB
/// per-bulk-string limit and Redis's ~1 GB command limit.
const BULK_BATCH_BYTES: usize = 128 * 1024 * 1024;

/// Per-call byte budget for batching, overridable via `CIH_BULK_BATCH_BYTES`
/// (tests force a tiny value to exercise the multi-call path deterministically).
pub(crate) fn batch_budget() -> usize {
    std::env::var("CIH_BULK_BATCH_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&b| b > 0)
        .unwrap_or(BULK_BATCH_BYTES)
}

/// Encode a streamed `Node` sequence into byte-budgeted `:Symbol` batches,
/// dropping each node after it is encoded. Dedups by id (first wins) and returns
/// the batches (`(count, blob)`, send in order — the first call carries `BEGIN`)
/// plus the owned `id → ordinal` map the edge pass needs. Ordinal = global
/// insertion index across all batches.
pub(crate) fn build_node_batches(
    nodes: impl Iterator<Item = io::Result<Node>>,
    budget: usize,
) -> io::Result<(Vec<Batch>, OrdinalMap)> {
    let mut ordinal: OrdinalMap = HashMap::new();
    let mut batches: Vec<Batch> = Vec::new();
    let mut blob = Vec::new();
    write_header(&mut blob, "Symbol", &NODE_PROPS);
    let mut in_batch = 0u64;
    let mut total = 0u64;
    for n in nodes {
        let n = n?;
        if ordinal.contains_key(n.id.as_str()) {
            continue;
        }
        ordinal.insert(n.id.as_str().to_string(), total);
        encode_node(&mut blob, &n);
        in_batch += 1;
        total += 1;
        if blob.len() >= budget {
            batches.push((in_batch, std::mem::take(&mut blob)));
            write_header(&mut blob, "Symbol", &NODE_PROPS);
            in_batch = 0;
        }
    }
    if in_batch > 0 {
        batches.push((in_batch, std::mem::take(&mut blob)));
    }
    Ok((batches, ordinal))
}

/// Encode a streamed `Edge` sequence into byte-budgeted per-reltype batches,
/// dropping each edge after it is encoded. Reproduces the Cypher graph exactly:
/// drops danglers (endpoint absent from `ordinal`) and dedups by
/// `(src_ord, dst_ord, kind)`. Holds one open blob per relationship type, flushed
/// at `budget`.
pub(crate) fn build_edge_batches(
    edges: impl Iterator<Item = io::Result<Edge>>,
    ordinal: &OrdinalMap,
    budget: usize,
) -> io::Result<Vec<Batch>> {
    let mut open: HashMap<EdgeKind, Batch> = HashMap::new();
    let mut seen: HashSet<(u64, u64, EdgeKind)> = HashSet::new();
    let mut batches: Vec<Batch> = Vec::new();
    for e in edges {
        let e = e?;
        let (src, dst) = match (ordinal.get(e.src.as_str()), ordinal.get(e.dst.as_str())) {
            (Some(&s), Some(&d)) => (s, d),
            _ => continue, // dangling endpoint — the Cypher MATCH drops these too
        };
        if !seen.insert((src, dst, e.kind)) {
            continue; // duplicate (src,dst,kind)
        }
        let slot = open.entry(e.kind).or_insert_with(|| {
            let mut blob = Vec::new();
            write_header(&mut blob, e.kind.cypher_label(), &EDGE_PROPS);
            (0, blob)
        });
        encode_edge_row(&mut slot.1, src, dst, &e);
        slot.0 += 1;
        if slot.1.len() >= budget {
            let full = open.remove(&e.kind).expect("slot just inserted");
            batches.push(full);
        }
    }
    for (_kind, batch) in open {
        if batch.0 > 0 {
            batches.push(batch);
        }
    }
    Ok(batches)
}

/// One edge row: `src_ord` (u64 LE) + `dst_ord` (u64 LE) + the `EDGE_PROPS`
/// values (confidence DOUBLE, reason STRING, callSites STRING/NULL).
fn encode_edge_row(blob: &mut Vec<u8>, src: u64, dst: u64, e: &Edge) {
    blob.extend_from_slice(&src.to_le_bytes());
    blob.extend_from_slice(&dst.to_le_bytes());
    push_double(blob, e.confidence as f64);
    push_string(blob, &e.reason);
    let call_sites = e
        .props
        .as_ref()
        .and_then(|p| p.get("call_sites"))
        .map(|v| v.to_string());
    push_opt_string(blob, call_sites.as_deref());
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{Node, NodeId, NodeKind, Range};

    fn node(id: &str) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind: NodeKind::Function,
            name: "n".to_string(),
            qualified_name: None,
            file: "f.rs".to_string(),
            range: Range::default(),
            props: None,
        }
    }

    fn edge(src: &str, dst: &str, kind: EdgeKind) -> Edge {
        Edge {
            src: NodeId::new(src.to_string()),
            dst: NodeId::new(dst.to_string()),
            kind,
            confidence: 1.0,
            reason: "r".to_string(),
            props: None,
        }
    }

    #[test]
    fn value_encoding_exact_bytes() {
        let mut b = Vec::new();
        push_string(&mut b, "ab");
        assert_eq!(b, vec![T_STRING, b'a', b'b', 0]);

        let mut b = Vec::new();
        push_long(&mut b, 1);
        assert_eq!(b, vec![T_LONG, 1, 0, 0, 0, 0, 0, 0, 0]); // i64 LE

        let mut b = Vec::new();
        push_double(&mut b, 1.0);
        assert_eq!(b[0], T_DOUBLE);
        assert_eq!(&b[1..], &1.0f64.to_le_bytes());

        let mut b = Vec::new();
        push_opt_string(&mut b, None);
        assert_eq!(b, vec![T_NULL]);
    }

    #[test]
    fn header_layout() {
        let mut b = Vec::new();
        write_header(&mut b, "Sym", &["a", "bb"]);
        // "Sym\0" + u32 LE count(2) + "a\0" + "bb\0"
        assert_eq!(
            b,
            vec![b'S', b'y', b'm', 0, 2, 0, 0, 0, b'a', 0, b'b', b'b', 0]
        );
    }

    const BIG: usize = 1 << 30; // 1 GiB — one batch

    /// Wrap owned items as the `io::Result`-yielding stream the builders take.
    fn stream<T>(items: Vec<T>) -> impl Iterator<Item = io::Result<T>> {
        items.into_iter().map(Ok)
    }

    fn node_count(batches: &[(u64, Vec<u8>)]) -> u64 {
        batches.iter().map(|(n, _)| n).sum()
    }

    #[test]
    fn dedup_nodes_and_dangling_edges() {
        // Two nodes, one a duplicate id; one valid edge, one dangling.
        let nodes = vec![node("A"), node("B"), node("A")];
        let edges = vec![
            edge("A", "B", EdgeKind::Calls),
            edge("A", "X", EdgeKind::Calls), // X absent → dropped
            edge("A", "B", EdgeKind::Calls), // duplicate → dropped
        ];
        let (nb, ord) = build_node_batches(stream(nodes), BIG).unwrap();
        assert_eq!(node_count(&nb), 2, "duplicate id collapsed");
        assert_eq!(nb.len(), 1);
        let eb = build_edge_batches(stream(edges), &ord, BIG).unwrap();
        assert_eq!(node_count(&eb), 1, "dangling + duplicate dropped");
        assert_eq!(eb.len(), 1);
    }

    #[test]
    fn edge_row_references_ordinals() {
        let (_nb, ord) = build_node_batches(stream(vec![node("A"), node("B")]), BIG).unwrap();
        let eb =
            build_edge_batches(stream(vec![edge("A", "B", EdgeKind::Calls)]), &ord, BIG).unwrap();
        let blob = &eb[0].1;
        // header = "CALLS\0" + u32(3) + "confidence\0reason\0callSites\0"
        let header_len = 6 + 4 + "confidence\0reason\0callSites\0".len();
        let row = &blob[header_len..];
        assert_eq!(&row[0..8], &0u64.to_le_bytes()); // src ordinal (A=0)
        assert_eq!(&row[8..16], &1u64.to_le_bytes()); // dst ordinal (B=1)
    }

    #[test]
    fn tiny_budget_splits_into_multiple_batches_preserving_ordinals() {
        // 6 nodes A..F, chained edges A->B->...->F, all CALLS. A tiny budget forces
        // several node + edge batches.
        let ids = ["A", "B", "C", "D", "E", "F"];
        let nodes: Vec<_> = ids.iter().map(|i| node(i)).collect();
        let edges: Vec<_> = ids
            .windows(2)
            .map(|w| edge(w[0], w[1], EdgeKind::Calls))
            .collect();
        let (nb, ord) = build_node_batches(stream(nodes), 1).unwrap();
        let eb = build_edge_batches(stream(edges), &ord, 1).unwrap();
        assert_eq!(node_count(&nb), 6);
        assert_eq!(node_count(&eb), 5);
        assert!(nb.len() > 1, "nodes split across batches");
        assert!(eb.len() > 1, "edges split across batches");
        // Ordinals are contiguous 0..6 in insertion order.
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(ord.get(*id), Some(&(i as u64)));
        }
    }

    #[test]
    fn stream_error_propagates() {
        let nodes: Vec<io::Result<Node>> = vec![
            Ok(node("A")),
            Err(io::Error::new(io::ErrorKind::InvalidData, "bad line")),
        ];
        let err = build_node_batches(nodes.into_iter(), BIG).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
