//! FalkorDB `GraphStore` + `BulkLoader` adapter (the open-source / dev backend).
//!
//! FalkorDB speaks the Redis protocol with a `GRAPH.QUERY <key> <openCypher>`
//! command, driven here via the `redis` crate. At go-live the same openCypher
//! queries move to a Neptune adapter (different driver: HTTPS + SigV4).
//!
//! All nodes carry a single `:Symbol` label + a `kind` property; the node `id`
//! is unique and indexed. Read queries pass the id via the FalkorDB `CYPHER`
//! parameter preamble (`CYPHER id=<lit> ... $id ...`) so the plan is cached and
//! the id is not concatenated ad-hoc into the match pattern. Bulk writes inline a
//! generated `UNWIND` list literal (our own data, fully escaped).

use std::collections::HashMap;

use async_trait::async_trait;
use cih_core::{
    Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId, NodeKind, Range, VersionId,
};
use cih_graph_store::{
    risk_from_fanout, BulkLoader, CommunityInfo, Direction, GraphStore, GraphStoreError, Impact,
    ImpactNode, LoadStats, Path, Result, Subgraph, SymbolContext,
};
use redis::Value;

/// Rows per UNWIND batch during bulk load.
const BATCH: usize = 1000;

pub struct FalkorStore {
    client: redis::Client,
    graph_key: String,
}

impl FalkorStore {
    pub fn connect(url: &str, graph_key: impl Into<String>) -> Result<Self> {
        let client =
            redis::Client::open(url).map_err(|e| GraphStoreError::Backend(e.to_string()))?;
        Ok(Self {
            client,
            graph_key: graph_key.into(),
        })
    }

    async fn run(&self, cypher: &str) -> Result<Value> {
        let mut con = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| GraphStoreError::Backend(e.to_string()))?;
        redis::cmd("GRAPH.QUERY")
            .arg(&self.graph_key)
            .arg(cypher)
            .query_async(&mut con)
            .await
            .map_err(|e| GraphStoreError::Backend(e.to_string()))
    }

    /// Result rows (the second element of a GRAPH.QUERY reply) as stringified
    /// cells. Good enough for scalar `RETURN` columns.
    async fn rows(&self, cypher: &str) -> Result<Vec<Vec<String>>> {
        let reply = self.run(cypher).await?;
        let top = as_array(&reply);
        let Some(rows_val) = top.get(1) else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for row in as_array(rows_val) {
            out.push(as_array(row).iter().map(cell_to_string).collect());
        }
        Ok(out)
    }

    /// Core write path: MERGE nodes then edges in UNWIND batches. Idempotent
    /// (re-running the same artifact is a no-op), so it doubles as upsert.
    async fn load_nodes_edges(&self, nodes: &[Node], edges: &[Edge]) -> Result<LoadStats> {
        let _ = self.run("CREATE INDEX FOR (n:Symbol) ON (n.id)").await; // idempotent

        for chunk in nodes.chunks(BATCH) {
            let q = format!(
                "UNWIND {arr} AS row \
                 MERGE (n:Symbol {{id: row.id}}) \
                 SET n.name = row.name, n.kind = row.kind, n.file = row.file, \
                     n.qualifiedName = row.qn, n.startLine = row.sl, n.endLine = row.el, \
                     n.props = row.props, n.stereotype = row.stereotype, \
                     n.httpMethod = row.httpMethod, n.path = row.path, \
                     n.decorator = row.decorator, n.handler = row.handler, \
                     n.symbolCount = row.symbolCount, n.cohesion = row.cohesion, \
                     n.processType = row.processType",
                arr = nodes_to_list(chunk)
            );
            self.run(&q).await?;
        }

        // Relationship types can't be parameterized in MERGE → one batch per kind.
        let mut by_kind: HashMap<EdgeKind, Vec<&Edge>> = HashMap::new();
        for e in edges {
            by_kind.entry(e.kind).or_default().push(e);
        }
        for (kind, es) in &by_kind {
            let label = kind.cypher_label();
            for chunk in es.chunks(BATCH) {
                let q = format!(
                    "UNWIND {arr} AS row \
                     MATCH (a:Symbol {{id: row.src}}), (b:Symbol {{id: row.dst}}) \
                     MERGE (a)-[r:{label}]->(b) \
                     SET r.confidence = row.conf, r.reason = row.reason",
                    arr = edges_to_list(chunk)
                );
                self.run(&q).await?;
            }
        }

        Ok(LoadStats {
            nodes: nodes.len() as u64,
            edges: edges.len() as u64,
        })
    }
}

#[async_trait]
impl GraphStore for FalkorStore {
    async fn ensure_schema(&self) -> Result<()> {
        let _ = self.run("CREATE INDEX FOR (n:Symbol) ON (n.id)").await;
        Ok(())
    }

    async fn bulk_load(&self, artifacts: &GraphArtifacts) -> Result<LoadStats> {
        let nodes = artifacts
            .read_nodes()
            .map_err(|e| GraphStoreError::Backend(format!("read nodes: {e}")))?;
        let edges = artifacts
            .read_edges()
            .map_err(|e| GraphStoreError::Backend(format!("read edges: {e}")))?;
        let stats = self.load_nodes_edges(&nodes, &edges).await?;
        self.swap_version(&artifacts.version).await?;
        Ok(stats)
    }

    async fn upsert_incremental(&self, delta: &GraphDelta) -> Result<()> {
        // Drop everything belonging to changed/removed files, then re-load the delta.
        let mut files: Vec<&String> = delta.changed_files.iter().collect();
        files.extend(delta.removed_files.iter());
        if !files.is_empty() {
            let list = format!(
                "[{}]",
                files.iter().map(|f| cstr(f)).collect::<Vec<_>>().join(", ")
            );
            self.run(&format!(
                "MATCH (n:Symbol) WHERE n.file IN {list} DETACH DELETE n"
            ))
            .await?;
        }
        self.load_nodes_edges(&delta.nodes, &delta.edges).await?;
        Ok(())
    }

    async fn swap_version(&self, version: &VersionId) -> Result<()> {
        // Pragmatic: record the version on a meta node. Production blue-green
        // (load into a staging graph key, then GRAPH.COPY/swap) is a later refinement.
        self.run(&format!(
            "MERGE (m:_CihMeta {{key:'version'}}) SET m.value = {}",
            cstr(&version.0)
        ))
        .await?;
        Ok(())
    }

    async fn get_node(&self, id: &NodeId) -> Result<Option<Node>> {
        let q = format!(
            "CYPHER id={id} \
             MATCH (n:Symbol {{id:$id}}) \
             RETURN n.id, labels(n)[0], n.name, n.qualifiedName, n.file LIMIT 1",
            id = cstr(id.as_str())
        );
        let rows = self.rows(&q).await?;
        Ok(rows.first().map(|r| node_from_row(r)))
    }

    async fn neighbors(
        &self,
        id: &NodeId,
        dir: Direction,
        kinds: &[EdgeKind],
    ) -> Result<Vec<Edge>> {
        let rel = rel_filter(kinds);
        let pat = match dir {
            Direction::Upstream => format!("(n:Symbol {{id:$id}})<-[r{rel}]-(m:Symbol)"),
            Direction::Downstream => format!("(n:Symbol {{id:$id}})-[r{rel}]->(m:Symbol)"),
            Direction::Both => format!("(n:Symbol {{id:$id}})-[r{rel}]-(m:Symbol)"),
        };
        let q = format!(
            "CYPHER id={id} MATCH {pat} RETURN type(r), n.id, m.id",
            id = cstr(id.as_str())
        );
        let rows = self.rows(&q).await?;
        Ok(rows
            .into_iter()
            .filter(|r| r.len() >= 3)
            .map(|r| Edge {
                kind: edge_from_label(&r[0]),
                src: NodeId::new(r[1].clone()),
                dst: NodeId::new(r[2].clone()),
                confidence: 1.0,
                reason: String::new(),
            })
            .collect())
    }

    async fn impact(&self, id: &NodeId, dir: Direction, max_depth: u32) -> Result<Impact> {
        let d = max_depth.clamp(1, 20);
        // Var-length bounds can't be parameterized; d is a clamped integer (safe).
        let arrow = match dir {
            Direction::Upstream => format!("<-[:CALLS*1..{d}]-"),
            Direction::Downstream => format!("-[:CALLS*1..{d}]->"),
            Direction::Both => format!("-[:CALLS*1..{d}]-"),
        };
        let q = format!(
            "CYPHER id={id} \
             MATCH p=(n:Symbol {{id:$id}}){arrow}(m:Symbol) \
             RETURN DISTINCT m.id, min(length(p))",
            id = cstr(id.as_str())
        );
        let rows = self.rows(&q).await?;
        let affected: Vec<ImpactNode> = rows
            .into_iter()
            .filter(|r| !r.is_empty())
            .map(|r| ImpactNode {
                id: NodeId::new(r[0].clone()),
                depth: r.get(1).and_then(|s| s.parse().ok()).unwrap_or(0),
                via: "CALLS".to_string(),
            })
            .collect();
        let risk = risk_from_fanout(affected.len()).to_string();
        Ok(Impact {
            root: id.clone(),
            direction: dir,
            affected,
            risk,
        })
    }

    async fn call_chain(&self, from: &NodeId, to: &NodeId, max_depth: u32) -> Result<Vec<Path>> {
        let d = max_depth.clamp(1, 12);
        let q = format!(
            "CYPHER from={from} to={to} \
             MATCH p=(a:Symbol {{id:$from}})-[:CALLS*1..{d}]->(b:Symbol {{id:$to}}) \
             RETURN [x IN nodes(p) | x.id] LIMIT 25",
            from = cstr(from.as_str()),
            to = cstr(to.as_str())
        );
        let rows = self.rows(&q).await?;
        Ok(rows
            .into_iter()
            .filter(|r| !r.is_empty())
            .map(|r| Path {
                nodes: r[0]
                    .trim_matches(|c| c == '[' || c == ']')
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| NodeId::new(s.trim().trim_matches('"').to_string()))
                    .collect(),
            })
            .collect())
    }

    async fn subgraph(&self, seeds: &[NodeId], radius: u32) -> Result<Subgraph> {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let r = radius.clamp(1, 4);
        for seed in seeds {
            if let Some(n) = self.get_node(seed).await? {
                nodes.push(n);
            }
            let q = format!(
                "CYPHER id={id} \
                 MATCH (n:Symbol {{id:$id}})-[*1..{r}]-(m:Symbol) \
                 RETURN DISTINCT m.id, labels(m)[0], m.name, m.qualifiedName, m.file LIMIT 200",
                id = cstr(seed.as_str())
            );
            for row in self.rows(&q).await? {
                nodes.push(node_from_row(&row));
            }
            edges.extend(self.neighbors(seed, Direction::Both, &[]).await?);
        }
        Ok(Subgraph { nodes, edges })
    }

    async fn context(&self, id: &NodeId) -> Result<SymbolContext> {
        let node = self
            .get_node(id)
            .await?
            .ok_or_else(|| GraphStoreError::NotFound(id.to_string()))?;
        let callers = neighbor_nodes(self, id, Direction::Upstream).await?;
        let callees = neighbor_nodes(self, id, Direction::Downstream).await?;
        let proc_query = format!(
            "CYPHER id={id} \
             MATCH (s:Symbol {{id:$id}})-[:STEP_IN_PROCESS]->(p:Symbol) \
             WHERE p.kind = 'Process' \
             RETURN p.id ORDER BY p.name",
            id = cstr(id.as_str())
        );
        let processes = self
            .rows(&proc_query)
            .await?
            .into_iter()
            .filter_map(|row| row.first().cloned())
            .collect();
        Ok(SymbolContext {
            node,
            callers,
            callees,
            processes,
        })
    }

    async fn communities(&self) -> Result<Vec<CommunityInfo>> {
        let q = "MATCH (c:Symbol) WHERE c.kind = 'Community' \
                 RETURN c.id, c.name, c.symbolCount, c.cohesion \
                 ORDER BY c.symbolCount DESC, c.name";
        Ok(self
            .rows(q)
            .await?
            .into_iter()
            .filter(|row| row.len() >= 2)
            .map(|row| CommunityInfo {
                id: row.first().cloned().unwrap_or_default(),
                name: row.get(1).cloned().unwrap_or_default(),
                symbol_count: row.get(2).and_then(|s| s.parse().ok()).unwrap_or(0),
                cohesion: row.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.0),
            })
            .collect())
    }
}

/// Thin `BulkLoader` over a `FalkorStore` (ports & adapters: the engine depends
/// on the `BulkLoader` trait, not on FalkorDB).
pub struct FalkorBulkLoader {
    store: FalkorStore,
}

impl FalkorBulkLoader {
    pub fn connect(url: &str, graph_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            store: FalkorStore::connect(url, graph_key)?,
        })
    }
}

#[async_trait]
impl BulkLoader for FalkorBulkLoader {
    async fn load(&self, artifacts: &GraphArtifacts) -> Result<LoadStats> {
        self.store.bulk_load(artifacts).await
    }
}

// ---- helpers ----

async fn neighbor_nodes(store: &FalkorStore, id: &NodeId, dir: Direction) -> Result<Vec<Node>> {
    let arrow = match dir {
        Direction::Upstream => "<-[:CALLS]-",
        Direction::Downstream => "-[:CALLS]->",
        Direction::Both => "-[:CALLS]-",
    };
    let q = format!(
        "CYPHER id={id} \
         MATCH (n:Symbol {{id:$id}}){arrow}(m:Symbol) \
         RETURN DISTINCT m.id, labels(m)[0], m.name, m.qualifiedName, m.file LIMIT 100",
        id = cstr(id.as_str())
    );
    Ok(store
        .rows(&q)
        .await?
        .iter()
        .map(|r| node_from_row(r))
        .collect())
}

fn node_from_row(r: &[String]) -> Node {
    Node {
        id: NodeId::new(r.first().cloned().unwrap_or_default()),
        kind: NodeKind::from_label(r.get(1).map(String::as_str).unwrap_or("")),
        name: r.get(2).cloned().unwrap_or_default(),
        qualified_name: r.get(3).filter(|s| !s.is_empty()).cloned(),
        file: r.get(4).cloned().unwrap_or_default(),
        range: Range::default(),
        props: None,
    }
}

fn nodes_to_list(nodes: &[Node]) -> String {
    let items: Vec<String> = nodes
        .iter()
        .map(|n| {
            let props_json = n.props.as_ref().map(serde_json::Value::to_string);
            let id = cstr(n.id.as_str());
            let name = cstr(&n.name);
            let kind = cstr(n.kind.label());
            let file = cstr(&n.file);
            let qn = copt(n.qualified_name.as_deref());
            let sl = n.range.start_line;
            let el = n.range.end_line;
            let props = copt(props_json.as_deref());
            let stereotype = copt(prop_str(n, "stereotype"));
            let http_method = copt(prop_str(n, "httpMethod"));
            let path = copt(prop_str(n, "path"));
            let decorator = copt(prop_str(n, "decorator"));
            let handler = copt(prop_str(n, "handler"));
            let symbol_count = cnum_u64(prop_u64(n, "symbolCount").or_else(|| prop_u64(n, "symbol_count")));
            let cohesion = cnum_f64(prop_f64(n, "cohesion"));
            let process_type = copt(prop_str(n, "process_type"));
            format!(
                "{{id:{id}, name:{name}, kind:{kind}, file:{file}, qn:{qn}, sl:{sl}, el:{el}, props:{props}, stereotype:{stereotype}, httpMethod:{http_method}, path:{path}, decorator:{decorator}, handler:{handler}, symbolCount:{symbol_count}, cohesion:{cohesion}, processType:{process_type}}}"
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

fn prop_str<'a>(node: &'a Node, key: &str) -> Option<&'a str> {
    node.props.as_ref()?.get(key)?.as_str()
}

fn prop_u64(node: &Node, key: &str) -> Option<u64> {
    node.props.as_ref()?.get(key)?.as_u64()
}

fn prop_f64(node: &Node, key: &str) -> Option<f64> {
    node.props.as_ref()?.get(key)?.as_f64()
}

fn cnum_u64(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "null".into())
}

fn cnum_f64(v: Option<f64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "null".into())
}

fn edges_to_list(edges: &[&Edge]) -> String {
    let items: Vec<String> = edges
        .iter()
        .map(|e| {
            format!(
                "{{src:{}, dst:{}, conf:{}, reason:{}}}",
                cstr(e.src.as_str()),
                cstr(e.dst.as_str()),
                e.confidence,
                cstr(&e.reason),
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

fn rel_filter(kinds: &[EdgeKind]) -> String {
    if kinds.is_empty() {
        String::new()
    } else {
        let labels: Vec<&str> = kinds.iter().map(|k| k.cypher_label()).collect();
        format!(":{}", labels.join("|"))
    }
}

fn edge_from_label(label: &str) -> EdgeKind {
    match label {
        "CONTAINS" => EdgeKind::Contains,
        "CALLS" => EdgeKind::Calls,
        "EXTENDS" => EdgeKind::Extends,
        "IMPLEMENTS" => EdgeKind::Implements,
        "HAS_METHOD" => EdgeKind::HasMethod,
        "HAS_FIELD" => EdgeKind::HasField,
        "IMPORTS" => EdgeKind::Imports,
        "ACCESSES" => EdgeKind::Accesses,
        "USES" => EdgeKind::Uses,
        "METHOD_OVERRIDES" => EdgeKind::MethodOverrides,
        "METHOD_IMPLEMENTS" => EdgeKind::MethodImplements,
        "MEMBER_OF" => EdgeKind::MemberOf,
        "STEP_IN_PROCESS" => EdgeKind::StepInProcess,
        "HANDLES_ROUTE" => EdgeKind::HandlesRoute,
        _ => EdgeKind::Other,
    }
}

/// Cypher string literal with escaping (`'...'`). Used both in the `CYPHER`
/// parameter preamble and inside generated UNWIND list literals.
fn cstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

/// Optional Cypher string literal → `'...'` or `null`.
fn copt(s: Option<&str>) -> String {
    match s {
        Some(v) => cstr(v),
        None => "null".to_string(),
    }
}

fn as_array(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(items) => items.iter().collect(),
        _ => vec![],
    }
}

fn cell_to_string(v: &&Value) -> String {
    match v {
        Value::Nil => String::new(),
        Value::Int(i) => i.to_string(),
        Value::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
        Value::SimpleString(s) => s.clone(),
        Value::Double(d) => d.to_string(),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(|x| cell_to_string(&x)).collect();
            format!("[{}]", inner.join(", "))
        }
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_kind_label_roundtrip() {
        for kind in [
            NodeKind::File,
            NodeKind::Folder,
            NodeKind::Class,
            NodeKind::Interface,
            NodeKind::Enum,
            NodeKind::Record,
            NodeKind::Annotation,
            NodeKind::Method,
            NodeKind::Function,
            NodeKind::Constructor,
            NodeKind::Field,
            NodeKind::Route,
            NodeKind::Community,
            NodeKind::Process,
            NodeKind::Other,
        ] {
            assert_eq!(NodeKind::from_label(kind.label()), kind);
        }
        assert_eq!(NodeKind::from_label("Unknown"), NodeKind::Other);
    }

    #[test]
    fn cstr_escapes_backslash_and_single_quote() {
        assert_eq!(cstr("a\\b's"), "'a\\\\b\\'s'");
        assert_eq!(cstr("line\nnext\tcell\rend"), "'line\\nnext\\tcell\\rend'");
    }

    #[test]
    fn risk_from_fanout_buckets() {
        assert_eq!(risk_from_fanout(0), "none");
        assert_eq!(risk_from_fanout(5), "low");
        assert_eq!(risk_from_fanout(20), "medium");
        assert_eq!(risk_from_fanout(75), "high");
        assert_eq!(risk_from_fanout(76), "critical");
    }
}
