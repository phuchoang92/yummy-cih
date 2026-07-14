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

use std::collections::HashMap;

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

/// The encoded `GRAPH.BULK` payload: entity counts plus the ordered binary blobs
/// (one `:Symbol` node blob, then one blob per non-empty relationship type).
pub(crate) struct BulkPayload {
    pub node_count: u64,
    pub edge_count: u64,
    pub node_blobs: Vec<Vec<u8>>,
    pub rel_blobs: Vec<Vec<u8>>,
}

/// Build the bulk payload from artifact nodes/edges, reproducing the Cypher
/// load's graph exactly (dedup + dangling drop).
pub(crate) fn build_payload(nodes: &[Node], edges: &[Edge]) -> BulkPayload {
    // Node blob: dedup by id (first wins), ordinal = insertion index.
    let mut ordinal: HashMap<&str, u64> = HashMap::with_capacity(nodes.len());
    let mut node_blob = Vec::new();
    write_header(&mut node_blob, "Symbol", &NODE_PROPS);
    let mut node_count = 0u64;
    for n in nodes {
        let id = n.id.as_str();
        if ordinal.contains_key(id) {
            continue;
        }
        ordinal.insert(id, node_count);
        encode_node(&mut node_blob, n);
        node_count += 1;
    }

    // Group edges by kind; each kind is one reltype blob. Dedup by (src,dst) and
    // drop edges whose endpoints are not in the node set.
    let mut by_kind: HashMap<EdgeKind, Vec<&Edge>> = HashMap::new();
    for e in edges {
        by_kind.entry(e.kind).or_default().push(e);
    }
    let mut edge_count = 0u64;
    let mut rel_blobs = Vec::new();
    for (kind, es) in by_kind {
        let mut blob = Vec::new();
        write_header(&mut blob, kind.cypher_label(), &EDGE_PROPS);
        let mut seen: std::collections::HashSet<(&str, &str)> = std::collections::HashSet::new();
        let mut wrote = false;
        for e in es {
            let (src, dst) = match (ordinal.get(e.src.as_str()), ordinal.get(e.dst.as_str())) {
                (Some(&s), Some(&d)) => (s, d),
                _ => continue, // dangling endpoint — Cypher MATCH drops these too
            };
            if !seen.insert((e.src.as_str(), e.dst.as_str())) {
                continue; // duplicate (src,dst,kind)
            }
            blob.extend_from_slice(&src.to_le_bytes());
            blob.extend_from_slice(&dst.to_le_bytes());
            push_double(&mut blob, e.confidence as f64);
            push_string(&mut blob, &e.reason);
            let call_sites = e
                .props
                .as_ref()
                .and_then(|p| p.get("call_sites"))
                .map(|v| v.to_string());
            push_opt_string(&mut blob, call_sites.as_deref());
            edge_count += 1;
            wrote = true;
        }
        if wrote {
            rel_blobs.push(blob);
        }
    }

    BulkPayload {
        node_count,
        edge_count,
        node_blobs: vec![node_blob],
        rel_blobs,
    }
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

    #[test]
    fn dedup_nodes_and_dangling_edges() {
        // Two nodes, one a duplicate id; one valid edge, one dangling.
        let nodes = vec![node("A"), node("B"), node("A")];
        let edges = vec![
            edge("A", "B", EdgeKind::Calls),
            edge("A", "X", EdgeKind::Calls), // X absent → dropped
            edge("A", "B", EdgeKind::Calls), // duplicate → dropped
        ];
        let p = build_payload(&nodes, &edges);
        assert_eq!(p.node_count, 2, "duplicate id collapsed");
        assert_eq!(p.edge_count, 1, "dangling + duplicate dropped");
        assert_eq!(p.node_blobs.len(), 1);
        assert_eq!(p.rel_blobs.len(), 1);
    }

    #[test]
    fn edge_row_references_ordinals() {
        let nodes = vec![node("A"), node("B")]; // A=0, B=1
        let edges = vec![edge("A", "B", EdgeKind::Calls)];
        let p = build_payload(&nodes, &edges);
        let blob = &p.rel_blobs[0];
        // header = "CALLS\0" + u32(3) + "confidence\0reason\0callSites\0"
        let header_len = 6 + 4 + "confidence\0reason\0callSites\0".len();
        let row = &blob[header_len..];
        assert_eq!(&row[0..8], &0u64.to_le_bytes()); // src ordinal
        assert_eq!(&row[8..16], &1u64.to_le_bytes()); // dst ordinal
    }
}
