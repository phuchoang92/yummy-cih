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
use cih_core::{Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId, NodeKind, Range};
use cih_graph_store::{
    risk_from_fanout, BulkLoader, CommunityEdge, CommunityInfo, Direction, FlowNode, GraphStore,
    GraphOverview, GraphOverviewEdge, GraphOverviewNode, GraphStoreError, Impact, ImpactNode,
    LoadStats, Path, Result, RouteInfo, Subgraph, SymbolContext,
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

    async fn graph_command(&self, command: &str, args: &[&str]) -> Result<Value> {
        let mut con = self
            .client
            .get_multiplexed_async_connection()
            .await
            .map_err(|e| GraphStoreError::Backend(e.to_string()))?;
        let mut cmd = redis::cmd(command);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.query_async(&mut con)
            .await
            .map_err(|e| GraphStoreError::Backend(e.to_string()))
    }

    pub async fn drop_graph(&self) -> Result<()> {
        self.graph_command("GRAPH.DELETE", &[&self.graph_key])
            .await?;
        Ok(())
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
        let _ = self.run("CREATE INDEX FOR (n:Symbol) ON (n.kind)").await;
        Ok(())
    }

    async fn bulk_load(&self, artifacts: &GraphArtifacts) -> Result<LoadStats> {
        let nodes = artifacts
            .read_nodes()
            .map_err(|e| GraphStoreError::Backend(format!("read nodes: {e}")))?;
        let edges = artifacts
            .read_edges()
            .map_err(|e| GraphStoreError::Backend(format!("read edges: {e}")))?;
        self.load_nodes_edges(&nodes, &edges).await
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

    async fn publish_to(&self, dest_key: &str) -> Result<()> {
        // Delete the destination first; ignore the error if it doesn't exist yet.
        let _ = self.graph_command("GRAPH.DELETE", &[dest_key]).await;
        self.graph_command("GRAPH.COPY", &[&self.graph_key, dest_key])
            .await?;
        Ok(())
    }

    async fn get_node(&self, id: &NodeId) -> Result<Option<Node>> {
        let q = format!(
            "CYPHER id={id} \
             MATCH (n:Symbol {{id:$id}}) \
             RETURN n.id, n.kind, n.name, n.qualifiedName, n.file LIMIT 1",
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
        // Two-step aggregation: order paths by length per node, then take the first
        // (shortest) parent and the minimum depth. This gives accurate parent tracking
        // for D3 diagram rendering without requiring a separate query.
        let q = format!(
            "CYPHER id={id} \
             MATCH p=(n:Symbol {{id:$id}}){arrow}(m:Symbol) \
             WITH m, length(p) AS len, nodes(p)[length(p)-1] AS pnode \
             ORDER BY m.id, len \
             WITH m, collect(pnode)[0] AS parent, min(len) AS depth \
             RETURN m.id, depth, parent.id, m.name, m.kind \
             LIMIT 200",
            id = cstr(id.as_str())
        );
        let rows = self.rows(&q).await?;
        let affected: Vec<ImpactNode> = rows
            .into_iter()
            .filter(|r| !r.is_empty())
            .map(|r| ImpactNode {
                id: NodeId::new(r[0].clone()),
                depth: r.get(1).and_then(|s| s.parse().ok()).unwrap_or(0),
                parent_id: r
                    .get(2)
                    .filter(|s| !s.is_empty())
                    .map(|s| NodeId::new(s.clone())),
                name: r.get(3).cloned().unwrap_or_default(),
                kind: r.get(4).cloned().unwrap_or_default(),
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
                 RETURN DISTINCT m.id, m.kind, m.name, m.qualifiedName, m.file LIMIT 200",
                id = cstr(seed.as_str())
            );
            for row in self.rows(&q).await? {
                nodes.push(node_from_row(&row));
            }
            edges.extend(self.neighbors(seed, Direction::Both, &[]).await?);
        }
        Ok(Subgraph { nodes, edges })
    }

    async fn graph_overview(&self, max_nodes: usize, max_edges: usize) -> Result<GraphOverview> {
        let max_nodes = max_nodes.max(1);
        let max_edges = max_edges.max(1);

        let total_nodes = self
            .rows("MATCH (n:Symbol) RETURN count(n)")
            .await?
            .first()
            .and_then(|row| row.first())
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(0);
        let total_edges = self
            .rows("MATCH (:Symbol)-[r]->(:Symbol) RETURN count(r)")
            .await?
            .first()
            .and_then(|row| row.first())
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(0);

        // Architectural/runtime nodes come first, then connected symbols. The
        // final id ordering makes the projection stable when degrees tie.
        let node_query = format!(
            "MATCH (n:Symbol) \
             OPTIONAL MATCH (n)-[r]-(:Symbol) \
             WITH n, count(r) AS degree, \
                  CASE n.kind \
                    WHEN 'Community' THEN 0 \
                    WHEN 'Process' THEN 1 \
                    WHEN 'Route' THEN 2 \
                    WHEN 'IntegrationRoute' THEN 3 \
                    WHEN 'MessageDestination' THEN 4 \
                    WHEN 'KafkaTopic' THEN 5 \
                    WHEN 'ExternalEndpoint' THEN 6 \
                    WHEN 'DbTable' THEN 7 \
                    WHEN 'DbQuery' THEN 8 \
                    ELSE 20 \
                  END AS priority \
             RETURN id(n), n.id, n.kind, n.name, n.qualifiedName, n.file, degree \
             ORDER BY priority ASC, degree DESC, n.id ASC \
             LIMIT {max_nodes}"
        );

        let mut internal_to_node = HashMap::<i64, NodeId>::with_capacity(max_nodes);
        let mut nodes = Vec::with_capacity(max_nodes.min(total_nodes as usize));
        for row in self.rows(&node_query).await? {
            if row.len() < 7 {
                continue;
            }
            let Ok(internal_id) = row[0].parse::<i64>() else {
                continue;
            };
            let id = NodeId::new(row[1].clone());
            internal_to_node.insert(internal_id, id.clone());
            nodes.push(GraphOverviewNode {
                node: Node {
                    id,
                    kind: NodeKind::from_label(&row[2]),
                    name: row[3].clone(),
                    qualified_name: row.get(4).filter(|value| !value.is_empty()).cloned(),
                    file: row.get(5).cloned().unwrap_or_default(),
                    range: Range::default(),
                    props: None,
                },
                degree: row[6].parse::<u64>().unwrap_or(0),
            });
        }

        let mut edges = Vec::new();
        let selected_internal_ids = internal_to_node.keys().copied().collect::<Vec<_>>();
        if !selected_internal_ids.is_empty() {
            let ids = selected_internal_ids
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let edge_limit = max_edges.saturating_add(1);
            let edge_query = format!(
                "MATCH (a:Symbol)-[r]->(b:Symbol) \
                 WHERE id(a) IN [{ids}] AND id(b) IN [{ids}] \
                 WITH a, b, r, CASE type(r) \
                    WHEN 'CALLS' THEN 0 \
                    WHEN 'HANDLES_ROUTE' THEN 1 \
                    WHEN 'EXTERNAL_CALL' THEN 2 \
                    WHEN 'PUBLISHES_EVENT' THEN 3 \
                    WHEN 'LISTENS_TO' THEN 4 \
                    WHEN 'INTEGRATION_LINK' THEN 5 \
                    WHEN 'IMPLEMENTS' THEN 6 \
                    WHEN 'EXTENDS' THEN 7 \
                    WHEN 'IMPORTS' THEN 8 \
                    ELSE 20 END AS priority \
                 RETURN id(a), id(b), type(r) \
                 ORDER BY priority ASC, a.id ASC, b.id ASC, type(r) ASC \
                 LIMIT {edge_limit}"
            );

            for row in self.rows(&edge_query).await? {
                if row.len() < 3 || edges.len() >= max_edges {
                    break;
                }
                let (Ok(source_internal), Ok(target_internal)) =
                    (row[0].parse::<i64>(), row[1].parse::<i64>())
                else {
                    continue;
                };
                let (Some(source), Some(target)) = (
                    internal_to_node.get(&source_internal),
                    internal_to_node.get(&target_internal),
                ) else {
                    continue;
                };
                edges.push(GraphOverviewEdge {
                    source: source.clone(),
                    target: target.clone(),
                    kind: edge_from_label(&row[2]),
                });
            }
        }

        let truncated = nodes.len() < total_nodes as usize || edges.len() < total_edges as usize;
        Ok(GraphOverview {
            nodes,
            edges,
            total_nodes,
            total_edges,
            truncated,
        })
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
        let community = self
            .symbol_communities(&[id.clone()])
            .await?
            .into_iter()
            .find_map(|(nid, info)| if &nid == id { Some(info) } else { None });
        Ok(SymbolContext {
            node,
            callers,
            callees,
            processes,
            community,
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

    async fn route_map(&self, prefix: Option<&str>, limit: usize) -> Result<Vec<RouteInfo>> {
        let prefix_val = prefix.unwrap_or("");
        let q = format!(
            "CYPHER prefix={prefix_lit} limit={limit} \
             MATCH (m:Symbol)-[:HANDLES_ROUTE]->(r:Symbol) \
             WHERE r.kind = 'Route' \
               AND ($prefix = '' OR r.path STARTS WITH $prefix) \
             RETURN r.path, r.httpMethod, r.decorator, r.handler, m.id, m.name, m.qualifiedName \
             ORDER BY r.path, r.httpMethod \
             LIMIT $limit",
            prefix_lit = cstr(prefix_val),
        );
        Ok(self
            .rows(&q)
            .await?
            .into_iter()
            .filter(|row| row.len() >= 6)
            .map(|row| RouteInfo {
                path: row.first().cloned().unwrap_or_default(),
                http_method: row.get(1).cloned().unwrap_or_default(),
                decorator: row.get(2).cloned().unwrap_or_default(),
                handler_id: NodeId::new(row.get(4).cloned().unwrap_or_default()),
                handler_name: row.get(5).cloned().unwrap_or_default(),
                handler_qualified: row.get(6).cloned().unwrap_or_default(),
            })
            .collect())
    }

    async fn candidates_by_name(&self, name: &str, limit: usize) -> Result<Vec<Node>> {
        let lim = limit.clamp(1, 50);
        // Use n.kind (stored property) not labels(n)[0] (always "Symbol") so
        // node_from_row gets the real kind string.
        let q = format!(
            "CYPHER name={name_lit} \
             MATCH (n:Symbol) WHERE n.name = $name \
             RETURN n.id, n.kind, n.name, n.qualifiedName, n.file \
             ORDER BY n.id LIMIT {lim}",
            name_lit = cstr(name),
        );
        Ok(self
            .rows(&q)
            .await?
            .iter()
            .map(|r| node_from_row(r))
            .collect())
    }

    async fn nodes_in_files(&self, files: &[String]) -> Result<Vec<Node>> {
        if files.is_empty() {
            return Ok(vec![]);
        }
        let list = format!(
            "[{}]",
            files.iter().map(|f| cstr(f)).collect::<Vec<_>>().join(", ")
        );
        // Limit to callable/structural kinds most useful for change-impact analysis.
        let q = format!(
            "MATCH (n:Symbol) \
             WHERE n.file IN {list} \
               AND n.kind IN ['Method', 'Constructor', 'Function', 'Class', 'Interface', 'Enum'] \
             RETURN n.id, n.kind, n.name, n.qualifiedName, n.file \
             ORDER BY n.file, n.id"
        );
        Ok(self
            .rows(&q)
            .await?
            .iter()
            .map(|r| node_from_row(r))
            .collect())
    }

    async fn processes_for_symbols(&self, symbol_ids: &[NodeId]) -> Result<Vec<String>> {
        if symbol_ids.is_empty() {
            return Ok(vec![]);
        }
        let list = format!(
            "[{}]",
            symbol_ids
                .iter()
                .map(|id| cstr(id.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let q = format!(
            "MATCH (s:Symbol)-[:STEP_IN_PROCESS]->(p:Symbol) \
             WHERE s.id IN {list} AND p.kind = 'Process' \
             RETURN DISTINCT p.id \
             ORDER BY p.id"
        );
        Ok(self
            .rows(&q)
            .await?
            .into_iter()
            .filter_map(|row| row.into_iter().next())
            .collect())
    }

    async fn flow_downstream(&self, entry: &NodeId, max_depth: u32) -> Result<Vec<FlowNode>> {
        let d = max_depth.clamp(1, 10);
        // Two-step aggregation picks the shortest-path parent for each node so the
        // Mermaid renderer can draw accurate edges between caller and callee.
        let q = format!(
            "CYPHER id={id} \
             MATCH p=(start:Symbol {{id:$id}})\
             -[:CALLS|HANDLES_ROUTE|EXTERNAL_CALL|PUBLISHES_EVENT|LISTENS_TO*1..{d}]->(m:Symbol) \
             WITH m, length(p) AS len, nodes(p)[length(p)-1] AS pnode \
             ORDER BY m.id, len \
             WITH m, collect(pnode)[0] AS parent, min(len) AS depth \
             RETURN m.id, m.kind, m.name, m.qualifiedName, m.file, depth, parent.id \
             ORDER BY depth, m.name LIMIT 100",
            id = cstr(entry.as_str())
        );
        Ok(self
            .rows(&q)
            .await?
            .into_iter()
            .filter(|r| r.len() >= 5)
            .map(|r| FlowNode {
                id: NodeId::new(r[0].clone()),
                kind: NodeKind::from_label(r[1].as_str()),
                name: r[2].clone(),
                qualified_name: r.get(3).filter(|s| !s.is_empty()).cloned(),
                file: r.get(4).cloned().unwrap_or_default(),
                depth: r.get(5).and_then(|s| s.parse().ok()).unwrap_or(1),
                parent_id: r
                    .get(6)
                    .filter(|s| !s.is_empty())
                    .map(|s| NodeId::new(s.clone())),
            })
            .collect())
    }

    async fn symbol_communities(&self, ids: &[NodeId]) -> Result<Vec<(NodeId, CommunityInfo)>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let list = format!(
            "[{}]",
            ids.iter()
                .map(|id| cstr(id.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let q = format!(
            "MATCH (n:Symbol)-[:MEMBER_OF]->(c:Symbol) \
             WHERE n.id IN {list} AND c.kind = 'Community' \
             RETURN n.id, c.id, c.name, c.symbolCount, c.cohesion"
        );
        Ok(self
            .rows(&q)
            .await?
            .into_iter()
            .filter(|r| r.len() >= 5)
            .map(|r| {
                (
                    NodeId::new(r[0].clone()),
                    CommunityInfo {
                        id: r[1].clone(),
                        name: r[2].clone(),
                        symbol_count: r[3].parse().unwrap_or(0),
                        cohesion: r[4].parse().unwrap_or(0.0),
                    },
                )
            })
            .collect())
    }

    async fn test_coverage(&self, id: &NodeId) -> Result<Vec<Node>> {
        let id_lit = cstr(id.as_str());
        // Direct TESTS edges to this symbol, plus TESTS edges to its owner class.
        let q = format!(
            "MATCH (t:Symbol)-[:TESTS]->(target:Symbol) \
             WHERE target.id = {id_lit} \
                OR EXISTS {{ \
                      MATCH (owner:Symbol)-[:HAS_METHOD]->(target2:Symbol) \
                      WHERE target2.id = {id_lit} AND owner.id = target.id \
                   }} \
             RETURN DISTINCT t.id, t.kind, t.name, t.qualifiedName, t.file \
             ORDER BY t.file, t.name \
             LIMIT 50"
        );
        Ok(self
            .rows(&q)
            .await?
            .iter()
            .map(|r| node_from_row(r))
            .collect())
    }

    async fn tests_for_files(&self, files: &[String]) -> Result<Vec<Node>> {
        if files.is_empty() {
            return Ok(vec![]);
        }
        let list = format!(
            "[{}]",
            files.iter().map(|f| cstr(f)).collect::<Vec<_>>().join(", ")
        );
        // Direct TESTS edges where the production target is in the changed files.
        let q = format!(
            "MATCH (t:Symbol)-[:TESTS]->(prod:Symbol) \
             WHERE prod.file IN {list} \
             RETURN DISTINCT t.id, t.kind, t.name, t.qualifiedName, t.file \
             ORDER BY t.file, t.name \
             LIMIT 200"
        );
        let mut results: Vec<Node> = self
            .rows(&q)
            .await?
            .iter()
            .map(|r| node_from_row(r))
            .collect();

        // Also catch test methods that CALL into the changed files (one-hop indirect).
        let q2 = format!(
            "MATCH (t:Symbol)-[:TESTS]->(:Symbol)-[:CALLS]->(prod:Symbol) \
             WHERE prod.file IN {list} \
             RETURN DISTINCT t.id, t.kind, t.name, t.qualifiedName, t.file \
             ORDER BY t.file, t.name \
             LIMIT 200"
        );
        let indirect: Vec<Node> = self
            .rows(&q2)
            .await?
            .iter()
            .map(|r| node_from_row(r))
            .collect();

        // Merge, dedup by id.
        let mut seen = std::collections::HashSet::new();
        results.retain(|n| seen.insert(n.id.clone()));
        for n in indirect {
            if seen.insert(n.id.clone()) {
                results.push(n);
            }
        }
        results.sort_by(|a, b| a.file.cmp(&b.file).then(a.name.cmp(&b.name)));
        Ok(results)
    }

    async fn untested_symbols(&self, file_prefix: &str, limit: usize) -> Result<Vec<Node>> {
        let lim = limit.clamp(1, 500);
        let prefix_lit = cstr(file_prefix);
        let q = format!(
            "MATCH (n:Symbol) \
             WHERE n.file STARTS WITH {prefix_lit} \
               AND n.kind IN ['Method', 'Class', 'Interface'] \
               AND NOT n.stereotype = 'test' \
               AND NOT EXISTS {{ MATCH (:Symbol)-[:TESTS]->(n) }} \
             RETURN n.id, n.kind, n.name, n.qualifiedName, n.file \
             ORDER BY n.file, n.name \
             LIMIT {lim}"
        );
        Ok(self
            .rows(&q)
            .await?
            .iter()
            .map(|r| node_from_row(r))
            .collect())
    }

    async fn community_graph(&self) -> Result<Vec<CommunityEdge>> {
        // Count CALLS edges that cross community boundaries. Each unit of weight
        // represents one caller→callee pair where caller and callee belong to
        // different communities. Capped at 500 pairs to avoid a mega-result.
        let q = "MATCH (a:Symbol)-[:MEMBER_OF]->(ca:Symbol), \
                       (b:Symbol)-[:MEMBER_OF]->(cb:Symbol) \
                 WHERE ca.kind = 'Community' AND cb.kind = 'Community' \
                   AND (a)-[:CALLS]->(b) AND ca.id <> cb.id \
                 RETURN ca.id, cb.id, count(*) AS weight \
                 LIMIT 500";
        Ok(self
            .rows(q)
            .await?
            .into_iter()
            .filter(|r| r.len() >= 3)
            .map(|r| CommunityEdge {
                src: r[0].clone(),
                dst: r[1].clone(),
                weight: r[2].parse().unwrap_or(0),
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
         RETURN DISTINCT m.id, m.kind, m.name, m.qualifiedName, m.file LIMIT 100",
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
        "PUBLISHES_EVENT" => EdgeKind::PublishesEvent,
        "LISTENS_TO" => EdgeKind::ListensTo,
        "EXTERNAL_CALL" => EdgeKind::ExternalCall,
        "TESTS" => EdgeKind::Tests,
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
mod tests;
