//! The `GraphStore` trait implementation for `LadybugStore` — every query is a
//! dialect port of the reference implementation in `cih-falkor/src/query.rs`.
//! Dialect deltas (all spike-verified): `label(r)` not `type(r)`; list
//! indexing is 1-based; bare `ORDER BY` inside `WITH` is rejected, so the
//! shortest-parent trick becomes native `* SHORTEST` recursion (the
//! `RecursiveRel` value carries interior nodes + rel labels — parent and hop
//! kind fall out of it); result caps match the reference exactly.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use cih_core::{Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId, NodeKind};
use cih_graph_store::{
    risk_from_fanout, CallSiteArgs, CommunityEdge, CommunityInfo, Direction, FlowEdge, FlowHop,
    FlowNode, GraphOverview, GraphOverviewEdge, GraphOverviewNode, GraphStore, GraphStoreError,
    GraphSummary, HotspotNode, Impact, ImpactNode, KindCount, LoadObserver, LoadStats,
    NoopObserver, Path, Result, RouteInfo, SimilarMethod, Subgraph, SymbolContext,
};
use lbug::{Connection, Value};

use crate::convert::{
    cell_f64, cell_opt_str, cell_str, cell_u64, cstr, node_from_row, recursive_rel,
};
use crate::{run_blocking, LadybugStore};

/// Collect a query's rows. Runs inside a `with_read_conn` closure.
fn rows(conn: &Connection, q: &str) -> Result<Vec<Vec<Value>>> {
    let result = conn
        .query(q)
        .map_err(|e| GraphStoreError::Backend(format!("query failed: {e}")))?;
    Ok(result.into_iter().collect())
}

fn rel_filter(kinds: &[EdgeKind]) -> String {
    if kinds.is_empty() {
        String::new()
    } else {
        let labels: Vec<&str> = kinds.iter().map(|k| k.cypher_label()).collect();
        format!(":{}", labels.join("|:"))
    }
}

fn edge_from_label(label: &str) -> EdgeKind {
    for kind in <EdgeKind as strum::IntoEnumIterator>::iter() {
        if kind.cypher_label() == label {
            return kind;
        }
    }
    EdgeKind::Other
}

/// `(id, kind, name, qn, file)` row → `FlowNode` at `depth` (parity with the
/// reference `route_handler_nodes` row shape).
fn flow_node_from_row(r: &[Value], depth: u32) -> FlowNode {
    FlowNode {
        id: NodeId::new(cell_str(&r[0])),
        kind: NodeKind::from_label(&cell_str(&r[1])),
        name: cell_str(&r[2]),
        qualified_name: r.get(3).and_then(cell_opt_str),
        file: r.get(4).map(cell_str).unwrap_or_default(),
        depth,
        parent_id: None,
    }
}

/// Assemble a route-entry flow: route at depth 0, handlers at depth 1 via
/// HANDLES_ROUTE, downstream shifted one level, deduped by id, capped at 100.
/// (Verbatim port of the reference implementation.)
fn assemble_route_flow(entry: &NodeId, handlers: Vec<(FlowNode, Vec<FlowHop>)>) -> Vec<FlowHop> {
    let route_name = entry
        .as_str()
        .strip_prefix("Route:")
        .unwrap_or(entry.as_str())
        .to_string();
    let mut hops: Vec<FlowHop> = vec![FlowHop {
        node: FlowNode {
            id: entry.clone(),
            kind: NodeKind::Route,
            name: route_name,
            qualified_name: None,
            file: String::new(),
            depth: 0,
            parent_id: None,
        },
        via: None,
    }];
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(entry.as_str().to_string());
    for (mut handler, sub) in handlers {
        if !seen.insert(handler.id.as_str().to_string()) {
            continue;
        }
        handler.depth = 1;
        handler.parent_id = Some(entry.clone());
        hops.push(FlowHop {
            node: handler,
            via: Some(FlowEdge {
                kind: "HANDLES_ROUTE".to_string(),
                call_sites: Vec::new(),
            }),
        });
        for mut hop in sub.into_iter().skip(1) {
            if !seen.insert(hop.node.id.as_str().to_string()) {
                continue;
            }
            hop.node.depth += 1;
            hops.push(hop);
        }
    }
    hops.truncate(100);
    hops
}

impl LadybugStore {
    /// Handlers of a Route node (inverse HANDLES_ROUTE), for flow entry.
    async fn route_handler_nodes(&self, route: &NodeId) -> Result<Vec<FlowNode>> {
        let q = format!(
            "MATCH (r:Symbol {{id: {id}}})<-[:HANDLES_ROUTE]-(h:Symbol) \
             RETURN h.id, h.kind, h.name, h.qn, h.file \
             ORDER BY h.name LIMIT 100",
            id = cstr(route.as_str())
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out
            .iter()
            .filter(|r| r.len() >= 3)
            .map(|r| flow_node_from_row(r, 1))
            .collect())
    }

    /// CALLS neighbors as full nodes (context callers/callees).
    async fn neighbor_nodes(&self, id: &NodeId, dir: Direction) -> Result<Vec<Node>> {
        let arrow = match dir {
            Direction::Upstream => "<-[:CALLS]-",
            Direction::Downstream => "-[:CALLS]->",
            Direction::Both => "-[:CALLS]-",
        };
        let q = format!(
            "MATCH (n:Symbol {{id: {id}}}){arrow}(m:Symbol) \
             RETURN DISTINCT m.id, m.kind, m.name, m.qn, m.file LIMIT 100",
            id = cstr(id.as_str())
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out.iter().map(|r| node_from_row(r)).collect())
    }

    async fn count_scalar(&self, q: &'static str) -> Result<u64> {
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, q))
            .await?;
        Ok(out
            .first()
            .and_then(|r| r.first())
            .map(cell_u64)
            .unwrap_or(0))
    }
}

#[async_trait]
impl GraphStore for LadybugStore {
    async fn ensure_schema(&self) -> Result<()> {
        if self.read_current().is_some() {
            return Ok(());
        }
        // Create the first version with the full DDL, flip CURRENT (schema
        // creation is the one build step whose "loaded" state is just the
        // empty schema), then release the RW lock so other processes can read.
        match self.write_handle().await {
            Ok((version, _db)) => {
                self.close_handle().await?;
                self.flip_current(&version)?;
                Ok(())
            }
            Err(e) => {
                // Two processes can race to create the same first version;
                // the loser's RW open fails on the winner's lock — possibly
                // BEFORE the winner has flipped CURRENT. Give the winner a
                // moment to finish before treating the error as real.
                for _ in 0..5 {
                    if self.read_current().is_some() {
                        tracing::debug!(error = %e, "lost ensure_schema race; graph exists");
                        return Ok(());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e)
            }
        }
    }

    async fn bulk_load(&self, artifacts: &GraphArtifacts) -> Result<LoadStats> {
        self.bulk_load_observed(artifacts, &NoopObserver).await
    }

    async fn bulk_load_observed(
        &self,
        artifacts: &GraphArtifacts,
        obs: &dyn LoadObserver,
    ) -> Result<LoadStats> {
        let (version, db) = self.write_handle().await?;
        let version_path = self.key_dir().join(&version);
        // The lbug API is synchronous and the observer is a borrow — run the
        // load inline. Both callers are CLI flows (engine analyze/discover
        // and `artifact bootstrap`) on a current-thread runtime where
        // blocking is expected; the server never bulk-loads in-process.
        let conn = Connection::new(&db)
            .map_err(|e| GraphStoreError::Backend(format!("ladybug connection: {e}")))?;
        let result = crate::bulk::load_observed(&conn, &version_path, artifacts, obs);
        drop(conn);
        match result {
            Ok(stats) => {
                // Only now — data loaded and checkpointed — make this version
                // the live one.
                self.flip_current(&version)?;
                Ok(stats)
            }
            Err(e) => {
                // Discard the half-built version so this store's own reads
                // fall back to CURRENT (the previous good version) instead of
                // short-circuiting onto the partial build.
                self.discard_handle().await;
                Err(e)
            }
        }
    }

    async fn upsert_incremental(&self, delta: &GraphDelta) -> Result<()> {
        // If this store is already the writer (a build in progress), apply in
        // place; otherwise copy-on-write a new version from the published one.
        let writable = {
            let state = self.state_is_writable().await;
            state
        };
        if !writable {
            self.begin_cow_version().await?;
        }
        let (_version, db) = self.write_handle().await?;
        let mut files: Vec<&String> = delta.changed_files.iter().collect();
        files.extend(delta.removed_files.iter());
        let file_list = format!(
            "[{}]",
            files.iter().map(|f| cstr(f)).collect::<Vec<_>>().join(", ")
        );
        let nodes = delta.nodes.clone();
        let edges = delta.edges.clone();
        run_blocking(move || {
            let conn = Connection::new(&db)
                .map_err(|e| GraphStoreError::Backend(format!("ladybug connection: {e}")))?;
            if !files_is_empty(&file_list) {
                conn.query(&format!(
                    "MATCH (n:Symbol) WHERE n.file IN {file_list} DETACH DELETE n"
                ))
                .map_err(|e| GraphStoreError::Backend(format!("delta delete: {e}")))?;
            }
            crate::bulk::merge_nodes_edges(&conn, &nodes, &edges, &NoopObserver)?;
            conn.query("CHECKPOINT")
                .map_err(|e| GraphStoreError::Backend(format!("checkpoint: {e}")))?;
            Ok(())
        })
        .await?;
        if !writable {
            // COW build complete: release the lock, then flip CURRENT so
            // readers rotate onto the new version.
            if let Some(version) = self.close_handle().await? {
                self.flip_current(&version)?;
                LadybugStore::gc_versions(&self.key_dir());
            }
        }
        Ok(())
    }

    async fn publish_to(&self, dest_key: &str) -> Result<()> {
        self.publish_to_impl(dest_key).await
    }

    async fn drop_graph(&self) -> Result<()> {
        // Never checkpoint here — the version dir may already be gone
        // (post-publish). Just release the handle and remove the tree.
        self.discard_handle().await;
        let dir = self.key_dir();
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(GraphStoreError::Backend(format!(
                "drop graph {}: {e}",
                dir.display()
            ))),
        }
    }

    async fn get_node(&self, id: &NodeId) -> Result<Option<Node>> {
        let q = format!(
            "MATCH (n:Symbol {{id: {id}}}) \
             RETURN n.id, n.kind, n.name, n.qn, n.file LIMIT 1",
            id = cstr(id.as_str())
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out.first().map(|r| node_from_row(r)))
    }

    async fn neighbors(
        &self,
        id: &NodeId,
        dir: Direction,
        kinds: &[EdgeKind],
    ) -> Result<Vec<Edge>> {
        let rel = rel_filter(kinds);
        // Direction is expressed by the pattern, so stored orientation is
        // known per query: upstream rows are m→n, downstream n→m. `src`/`dst`
        // always reflect the stored edge (contract guarantee).
        let mut queries: Vec<(String, bool /* src is m */)> = Vec::new();
        let id_lit = cstr(id.as_str());
        if matches!(dir, Direction::Upstream | Direction::Both) {
            queries.push((
                format!(
                    "MATCH (n:Symbol {{id: {id_lit}}})<-[r{rel}]-(m:Symbol) \
                     RETURN label(r), m.id, n.id"
                ),
                true,
            ));
        }
        if matches!(dir, Direction::Downstream | Direction::Both) {
            queries.push((
                format!(
                    "MATCH (n:Symbol {{id: {id_lit}}})-[r{rel}]->(m:Symbol) \
                     RETURN label(r), n.id, m.id"
                ),
                false,
            ));
        }
        let mut edges = Vec::new();
        for (q, _) in queries {
            let out = self
                .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
                .await?;
            edges.extend(out.into_iter().filter(|r| r.len() >= 3).map(|r| Edge {
                kind: edge_from_label(&cell_str(&r[0])),
                src: NodeId::new(cell_str(&r[1])),
                dst: NodeId::new(cell_str(&r[2])),
                confidence: 1.0,
                reason: String::new(),
                props: None,
            }));
        }
        Ok(edges)
    }

    async fn impact(&self, id: &NodeId, dir: Direction, max_depth: u32) -> Result<Impact> {
        let d = max_depth.clamp(1, 20);
        let arrow = match dir {
            Direction::Upstream => format!("<-[e:CALLS* SHORTEST 1..{d}]-"),
            Direction::Downstream => format!("-[e:CALLS* SHORTEST 1..{d}]->"),
            Direction::Both => format!("-[e:CALLS* SHORTEST 1..{d}]-"),
        };
        // `* SHORTEST` yields one shortest path per (n, m) pair, replacing the
        // reference's ORDER BY + collect()[0] min-depth trick (bare ORDER BY
        // in WITH is rejected in this dialect). Parent of m = last interior
        // node of the path, or the root when it's a direct edge.
        let q = format!(
            "MATCH (n:Symbol {{id: {id}}}){arrow}(m:Symbol) \
             RETURN m.id, e, m.name, m.kind LIMIT 200",
            id = cstr(id.as_str())
        );
        let root = id.as_str().to_string();
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        let affected: Vec<ImpactNode> = out
            .iter()
            .filter(|r| r.len() >= 4)
            .filter_map(|r| {
                let (depth, interior, _labels) = recursive_rel(&r[1])?;
                let parent = interior.last().cloned().unwrap_or_else(|| root.clone());
                Some(ImpactNode {
                    id: NodeId::new(cell_str(&r[0])),
                    depth,
                    parent_id: Some(NodeId::new(parent)),
                    name: cell_str(&r[2]),
                    kind: cell_str(&r[3]),
                    via: "CALLS".to_string(),
                })
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
            "MATCH (a:Symbol {{id: {from}}})-[e:CALLS*1..{d}]->(b:Symbol {{id: {to}}}) \
             RETURN e LIMIT 25",
            from = cstr(from.as_str()),
            to = cstr(to.as_str())
        );
        let (from_id, to_id) = (from.clone(), to.clone());
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out
            .iter()
            .filter_map(|r| {
                let (_len, interior, _labels) = recursive_rel(r.first()?)?;
                let mut nodes = vec![from_id.clone()];
                nodes.extend(interior.into_iter().map(NodeId::new));
                nodes.push(to_id.clone());
                Some(Path { nodes })
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
                "MATCH (n:Symbol {{id: {id}}})-[e*1..{r}]-(m:Symbol) \
                 RETURN DISTINCT m.id, m.kind, m.name, m.qn, m.file LIMIT 200",
                id = cstr(seed.as_str())
            );
            let out = self
                .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
                .await?;
            nodes.extend(out.iter().map(|row| node_from_row(row)));
            edges.extend(self.neighbors(seed, Direction::Both, &[]).await?);
        }
        Ok(Subgraph { nodes, edges })
    }

    async fn graph_summary(&self) -> Result<GraphSummary> {
        let total_nodes = self
            .count_scalar("MATCH (n:Symbol) RETURN count(n)")
            .await?;
        let total_edges = self
            .count_scalar("MATCH (:Symbol)-[r]->(:Symbol) RETURN count(r)")
            .await?;
        let out = self
            .with_read_conn(Vec::new(), move |conn| {
                rows(
                    conn,
                    "MATCH (n:Symbol) RETURN n.kind, count(n) ORDER BY count(n) DESC",
                )
            })
            .await?;
        let kinds = out
            .into_iter()
            .filter(|r| r.len() >= 2)
            .map(|r| KindCount {
                kind: cell_str(&r[0]),
                count: cell_u64(&r[1]),
            })
            .collect();
        Ok(GraphSummary {
            kinds,
            total_nodes,
            total_edges,
        })
    }

    async fn graph_overview(
        &self,
        max_nodes: usize,
        max_edges: usize,
        kinds: Option<&[String]>,
    ) -> Result<GraphOverview> {
        let max_nodes = max_nodes.max(1);
        let max_edges = max_edges.max(1);
        let total_nodes = self
            .count_scalar("MATCH (n:Symbol) RETURN count(n)")
            .await?;
        let total_edges = self
            .count_scalar("MATCH (:Symbol)-[r]->(:Symbol) RETURN count(r)")
            .await?;

        // n.id (the string PK) is the selection key — no internal-id detour.
        let mut selected = HashSet::<String>::new();
        let mut nodes = Vec::new();
        fn push_row(
            nodes: &mut Vec<GraphOverviewNode>,
            selected: &mut HashSet<String>,
            r: &[Value],
            degree: u64,
        ) {
            let id = cell_str(&r[0]);
            if !selected.insert(id) {
                return;
            }
            nodes.push(GraphOverviewNode {
                node: node_from_row(r),
                degree,
            });
        }

        if let Some(kind_list) = kinds {
            let kind_literals = kind_list
                .iter()
                .map(|k| cstr(k))
                .collect::<Vec<_>>()
                .join(",");
            let q = format!(
                "MATCH (n:Symbol) WHERE n.kind IN [{kind_literals}] \
                 OPTIONAL MATCH (n)-[r]-(:Symbol) \
                 WITH n, count(r) AS degree ORDER BY degree DESC, n.id ASC LIMIT {max_nodes} \
                 RETURN n.id, n.kind, n.name, n.qn, n.file, degree"
            );
            let out = self
                .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
                .await?;
            for r in out.iter().filter(|r| r.len() >= 6) {
                push_row(&mut nodes, &mut selected, r, cell_u64(&r[5]));
            }
        } else {
            let structural = "['Community','Process','Route','IntegrationRoute',\
                 'MessageDestination','KafkaTopic','ExternalEndpoint','DbTable','DbQuery']";
            let pass1_limit = max_nodes.min(2_000);
            let q1 = format!(
                "MATCH (n:Symbol) WHERE n.kind IN {structural} \
                 RETURN n.id, n.kind, n.name, n.qn, n.file LIMIT {pass1_limit}"
            );
            let out = self
                .with_read_conn(Vec::new(), move |conn| rows(conn, &q1))
                .await?;
            for r in out.iter().filter(|r| r.len() >= 5) {
                push_row(&mut nodes, &mut selected, r, 0);
            }
            let remaining = max_nodes.saturating_sub(nodes.len());
            if remaining > 0 {
                let q2 = format!(
                    "MATCH (n:Symbol) WHERE n.kind IN ['Class','Interface','Enum','Record'] \
                     OPTIONAL MATCH (n)-[r]-(:Symbol) \
                     WITH n, count(r) AS degree ORDER BY degree DESC, n.id ASC LIMIT {remaining} \
                     RETURN n.id, n.kind, n.name, n.qn, n.file, degree"
                );
                let out = self
                    .with_read_conn(Vec::new(), move |conn| rows(conn, &q2))
                    .await?;
                for r in out.iter().filter(|r| r.len() >= 6) {
                    push_row(&mut nodes, &mut selected, r, cell_u64(&r[5]));
                }
            }
        }

        let mut edges = Vec::new();
        if !selected.is_empty() {
            let ids = selected
                .iter()
                .map(|s| cstr(s))
                .collect::<Vec<_>>()
                .join(",");
            let edge_limit = max_edges.saturating_add(1);
            let q = format!(
                "MATCH (a:Symbol)-[r]->(b:Symbol) \
                 WHERE a.id IN [{ids}] AND b.id IN [{ids}] \
                 WITH a, b, r, CASE label(r) \
                    WHEN 'CALLS' THEN 0 WHEN 'HANDLES_ROUTE' THEN 1 \
                    WHEN 'EXTERNAL_CALL' THEN 2 WHEN 'PUBLISHES_EVENT' THEN 3 \
                    WHEN 'LISTENS_TO' THEN 4 WHEN 'INTEGRATION_LINK' THEN 5 \
                    WHEN 'IMPLEMENTS' THEN 6 WHEN 'EXTENDS' THEN 7 \
                    WHEN 'IMPORTS' THEN 8 ELSE 20 END AS priority \
                 RETURN a.id, b.id, label(r), priority \
                 ORDER BY priority ASC, a.id ASC, b.id ASC LIMIT {edge_limit}"
            );
            let out = self
                .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
                .await?;
            for r in out.iter().filter(|r| r.len() >= 3) {
                if edges.len() >= max_edges {
                    break;
                }
                edges.push(GraphOverviewEdge {
                    source: NodeId::new(cell_str(&r[0])),
                    target: NodeId::new(cell_str(&r[1])),
                    kind: edge_from_label(&cell_str(&r[2])),
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
        let proc_q = format!(
            "MATCH (s:Symbol {{id: {id}}})-[:STEP_IN_PROCESS]->(p:Symbol) \
             WHERE p.kind = 'Process' RETURN p.id ORDER BY p.name",
            id = cstr(id.as_str())
        );
        let processes = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &proc_q))
            .await?
            .into_iter()
            .filter_map(|r| r.first().map(cell_str))
            .collect();
        let callers = self.neighbor_nodes(id, Direction::Upstream).await?;
        let callees = self.neighbor_nodes(id, Direction::Downstream).await?;
        let community = self
            .symbol_communities(std::slice::from_ref(id))
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
        let out = self
            .with_read_conn(Vec::new(), move |conn| {
                rows(
                    conn,
                    "MATCH (c:Symbol) WHERE c.kind = 'Community' \
                     RETURN c.id, c.name, c.symbolCount, c.cohesion \
                     ORDER BY c.symbolCount DESC, c.name",
                )
            })
            .await?;
        Ok(out
            .into_iter()
            .filter(|r| r.len() >= 2)
            .map(|r| CommunityInfo {
                id: cell_str(&r[0]),
                name: cell_str(&r[1]),
                symbol_count: r.get(2).map(cell_u64).unwrap_or(0),
                cohesion: r.get(3).map(cell_f64).unwrap_or(0.0),
            })
            .collect())
    }

    async fn route_map(&self, prefix: Option<&str>, limit: usize) -> Result<Vec<RouteInfo>> {
        let prefix_filter = match prefix.filter(|p| !p.is_empty()) {
            Some(p) => format!("AND r.path STARTS WITH {} ", cstr(p)),
            None => String::new(),
        };
        let q = format!(
            "MATCH (m:Symbol)-[:HANDLES_ROUTE]->(r:Symbol) \
             WHERE r.kind = 'Route' {prefix_filter}\
             RETURN r.path, r.httpMethod, r.decorator, r.handler, m.id, m.name, m.qn \
             ORDER BY r.path, r.httpMethod LIMIT {limit}",
            limit = limit.max(1)
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out
            .into_iter()
            .filter(|r| r.len() >= 6)
            .map(|r| RouteInfo {
                path: cell_str(&r[0]),
                http_method: cell_str(&r[1]),
                decorator: cell_str(&r[2]),
                handler_id: NodeId::new(r.get(4).map(cell_str).unwrap_or_default()),
                handler_name: r.get(5).map(cell_str).unwrap_or_default(),
                handler_qualified: r.get(6).map(cell_str).unwrap_or_default(),
            })
            .collect())
    }

    async fn candidates_by_name(&self, name: &str, limit: usize) -> Result<Vec<Node>> {
        let lim = limit.clamp(1, 50);
        let q = format!(
            "MATCH (n:Symbol) WHERE n.name = {name} \
             RETURN n.id, n.kind, n.name, n.qn, n.file ORDER BY n.id LIMIT {lim}",
            name = cstr(name)
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out.iter().map(|r| node_from_row(r)).collect())
    }

    async fn nodes_in_files(&self, files: &[String]) -> Result<Vec<Node>> {
        if files.is_empty() {
            return Ok(vec![]);
        }
        let list = format!(
            "[{}]",
            files.iter().map(|f| cstr(f)).collect::<Vec<_>>().join(", ")
        );
        let q = format!(
            "MATCH (n:Symbol) WHERE n.file IN {list} \
               AND n.kind IN ['Method', 'Constructor', 'Function', 'Class', 'Interface', 'Enum'] \
             RETURN n.id, n.kind, n.name, n.qn, n.file ORDER BY n.file, n.id"
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out.iter().map(|r| node_from_row(r)).collect())
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
             RETURN DISTINCT p.id ORDER BY p.id"
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out
            .into_iter()
            .filter_map(|r| r.first().map(cell_str))
            .collect())
    }

    async fn flow_downstream(&self, entry: &NodeId, max_depth: u32) -> Result<Vec<FlowHop>> {
        let d = max_depth.clamp(1, 10);

        // Route entry: hop route→handler via inverse HANDLES_ROUTE first.
        let handlers = self.route_handler_nodes(entry).await?;
        if !handlers.is_empty() {
            let mut with_downstream = Vec::with_capacity(handlers.len());
            for handler in handlers {
                let sub = self.flow_downstream(&handler.id, d).await?;
                with_downstream.push((handler, sub));
            }
            return Ok(assemble_route_flow(entry, with_downstream));
        }

        // One shortest path per reachable node; depth, parent, and the
        // incoming hop kind all come from the RecursiveRel value.
        let q = format!(
            "MATCH (start:Symbol {{id: {id}}})\
             -[e:CALLS|:HANDLES_ROUTE|:EXTERNAL_CALL|:PUBLISHES_EVENT|:LISTENS_TO* SHORTEST 1..{d}]->\
             (m:Symbol) \
             RETURN m.id, m.kind, m.name, m.qn, m.file, e LIMIT 100",
            id = cstr(entry.as_str())
        );
        let root = entry.as_str().to_string();
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;

        struct Reached {
            node: FlowNode,
            etype: String,
        }
        let mut reached: Vec<Reached> = out
            .iter()
            .filter(|r| r.len() >= 6)
            .filter_map(|r| {
                let (depth, interior, labels) = recursive_rel(&r[5])?;
                let parent = interior.last().cloned().unwrap_or_else(|| root.clone());
                let mut node = flow_node_from_row(r, depth);
                node.parent_id = Some(NodeId::new(parent));
                Some(Reached {
                    node,
                    etype: labels.last().cloned().unwrap_or_else(|| "CALLS".into()),
                })
            })
            .collect();
        reached.sort_by(|a, b| {
            a.node
                .depth
                .cmp(&b.node.depth)
                .then_with(|| a.node.name.cmp(&b.node.name))
        });
        reached.truncate(100);

        // Batch-fetch CALLS call-site args per (parent, child) pair.
        let pairs: Vec<(String, String)> = reached
            .iter()
            .filter_map(|hop| {
                hop.node
                    .parent_id
                    .as_ref()
                    .map(|p| (p.as_str().to_string(), hop.node.id.as_str().to_string()))
            })
            .collect();
        let mut call_sites_map: HashMap<(String, String), Vec<CallSiteArgs>> = HashMap::new();
        if !pairs.is_empty() {
            let pair_list = pairs
                .iter()
                .map(|(s, dst)| format!("[{}, {}]", cstr(s), cstr(dst)))
                .collect::<Vec<_>>()
                .join(", ");
            // List indexing is 1-based in this dialect: pair[1] = src.
            let eq = format!(
                "UNWIND [{pair_list}] AS pair \
                 MATCH (a:Symbol)-[r:CALLS]->(b:Symbol) \
                 WHERE a.id = pair[1] AND b.id = pair[2] \
                 RETURN a.id, b.id, r.callSites"
            );
            if let Ok(edge_rows) = self
                .with_read_conn(Vec::new(), move |conn| rows(conn, &eq))
                .await
            {
                for row in edge_rows.iter().filter(|r| r.len() >= 3) {
                    let cs_json = cell_str(&row[2]);
                    if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&cs_json) {
                        let call_sites: Vec<CallSiteArgs> = arr
                            .iter()
                            .filter_map(|v| {
                                let args = v.get("args")?.as_array()?;
                                Some(CallSiteArgs {
                                    args: args
                                        .iter()
                                        .filter_map(|a| a.as_str().map(|s| s.to_string()))
                                        .collect(),
                                })
                            })
                            .collect();
                        call_sites_map.insert((cell_str(&row[0]), cell_str(&row[1])), call_sites);
                    }
                }
            }
        }

        let entry_node = FlowNode {
            id: entry.clone(),
            kind: NodeKind::Method,
            name: entry.as_str().rsplit('#').next().unwrap_or("").to_string(),
            qualified_name: None,
            file: String::new(),
            depth: 0,
            parent_id: None,
        };
        let mut hops: Vec<FlowHop> = vec![FlowHop {
            node: entry_node,
            via: None,
        }];
        for r in reached {
            let via = r.node.parent_id.as_ref().map(|parent_id| {
                let key = (
                    parent_id.as_str().to_string(),
                    r.node.id.as_str().to_string(),
                );
                FlowEdge {
                    kind: r.etype.clone(),
                    call_sites: call_sites_map.remove(&key).unwrap_or_default(),
                }
            });
            hops.push(FlowHop { node: r.node, via });
        }
        Ok(hops)
    }

    async fn complexity_hotspots(
        &self,
        min_cyclomatic: Option<u16>,
        min_cognitive: Option<u16>,
        min_transitive_loop: Option<u8>,
        limit: usize,
    ) -> Result<Vec<HotspotNode>> {
        let min_cc = min_cyclomatic.unwrap_or(5) as i64;
        let min_cog = min_cognitive.unwrap_or(0) as i64;
        let min_tl = min_transitive_loop.unwrap_or(1) as i64;
        let lim = limit.clamp(1, 200) as i64;
        let q = format!(
            "MATCH (n:Symbol) WHERE n.kind IN ['Method', 'Constructor'] \
             AND n.transitiveLoopDepth >= {min_tl} \
             AND n.cyclomatic >= {min_cc} \
             AND n.cognitive >= {min_cog} \
             RETURN n.id, n.name, n.file, n.cyclomatic, n.cognitive, n.transitiveLoopDepth \
             ORDER BY n.transitiveLoopDepth DESC, n.cyclomatic DESC LIMIT {lim}"
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out
            .into_iter()
            .filter(|r| r.len() >= 6)
            .map(|r| HotspotNode {
                id: NodeId::new(cell_str(&r[0])),
                name: cell_str(&r[1]),
                file: cell_str(&r[2]),
                cyclomatic: cell_u64(&r[3]),
                cognitive: cell_u64(&r[4]),
                transitive_loop_depth: cell_u64(&r[5]),
            })
            .collect())
    }

    async fn similar_methods(
        &self,
        id: &NodeId,
        _min_jaccard: f32,
        limit: usize,
    ) -> Result<Vec<SimilarMethod>> {
        let lim = limit.clamp(1, 50) as i64;
        let q = format!(
            "MATCH (a:Symbol {{id: {id}}})-[r:SIMILAR_TO]->(b:Symbol) \
             RETURN b.id, b.name, b.file, r.confidence \
             ORDER BY r.confidence DESC LIMIT {lim}",
            id = cstr(id.as_str())
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out
            .into_iter()
            .filter(|r| r.len() >= 4)
            .map(|r| SimilarMethod {
                id: NodeId::new(cell_str(&r[0])),
                name: cell_str(&r[1]),
                file: cell_str(&r[2]),
                jaccard: cell_f64(&r[3]) as f32,
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
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out
            .into_iter()
            .filter(|r| r.len() >= 5)
            .map(|r| {
                (
                    NodeId::new(cell_str(&r[0])),
                    CommunityInfo {
                        id: cell_str(&r[1]),
                        name: cell_str(&r[2]),
                        symbol_count: cell_u64(&r[3]),
                        cohesion: cell_f64(&r[4]),
                    },
                )
            })
            .collect())
    }

    async fn test_coverage(&self, id: &NodeId) -> Result<Vec<Node>> {
        let id_lit = cstr(id.as_str());
        let q = format!(
            "MATCH (t:Symbol)-[:TESTS]->(target:Symbol) \
             WHERE target.id = {id_lit} \
                OR EXISTS {{ \
                      MATCH (owner:Symbol)-[:HAS_METHOD]->(target2:Symbol) \
                      WHERE target2.id = {id_lit} AND owner.id = target.id \
                   }} \
             RETURN DISTINCT t.id, t.kind, t.name, t.qn, t.file \
             ORDER BY t.file, t.name LIMIT 50"
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out.iter().map(|r| node_from_row(r)).collect())
    }

    async fn tests_for_files(&self, files: &[String]) -> Result<Vec<Node>> {
        if files.is_empty() {
            return Ok(vec![]);
        }
        let list = format!(
            "[{}]",
            files.iter().map(|f| cstr(f)).collect::<Vec<_>>().join(", ")
        );
        let q1 = format!(
            "MATCH (t:Symbol)-[:TESTS]->(prod:Symbol) WHERE prod.file IN {list} \
             RETURN DISTINCT t.id, t.kind, t.name, t.qn, t.file \
             ORDER BY t.file, t.name LIMIT 200"
        );
        let q2 = format!(
            "MATCH (t:Symbol)-[:TESTS]->(:Symbol)-[:CALLS]->(prod:Symbol) \
             WHERE prod.file IN {list} \
             RETURN DISTINCT t.id, t.kind, t.name, t.qn, t.file \
             ORDER BY t.file, t.name LIMIT 200"
        );
        let direct = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q1))
            .await?;
        let indirect = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q2))
            .await?;
        let mut results: Vec<Node> = direct.iter().map(|r| node_from_row(r)).collect();
        let mut seen = HashSet::new();
        results.retain(|n| seen.insert(n.id.clone()));
        for n in indirect.iter().map(|r| node_from_row(r)) {
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
        // `stereotype IS NULL OR <> 'test'` spells out the intended semantics
        // (a missing stereotype is not a test) rather than relying on
        // three-valued NOT like the reference query.
        let q = format!(
            "MATCH (n:Symbol) \
             WHERE n.file STARTS WITH {prefix_lit} \
               AND n.kind IN ['Method', 'Class', 'Interface'] \
               AND (n.stereotype IS NULL OR n.stereotype <> 'test') \
               AND NOT EXISTS {{ MATCH (:Symbol)-[:TESTS]->(n) }} \
             RETURN n.id, n.kind, n.name, n.qn, n.file \
             ORDER BY n.file, n.name LIMIT {lim}"
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out.iter().map(|r| node_from_row(r)).collect())
    }

    async fn community_graph(&self) -> Result<Vec<CommunityEdge>> {
        // Single-pattern rewrite of the reference's pattern-predicate WHERE
        // (`(a)-[:CALLS]->(b)` predicates aren't supported here).
        let q = "MATCH (ca:Symbol)<-[:MEMBER_OF]-(a:Symbol)-[:CALLS]->(b:Symbol)\
                 -[:MEMBER_OF]->(cb:Symbol) \
                 WHERE ca.kind = 'Community' AND cb.kind = 'Community' AND ca.id <> cb.id \
                 RETURN ca.id, cb.id, count(*) LIMIT 500";
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, q))
            .await?;
        Ok(out
            .into_iter()
            .filter(|r| r.len() >= 3)
            .map(|r| CommunityEdge {
                src: cell_str(&r[0]),
                dst: cell_str(&r[1]),
                weight: cell_u64(&r[2]),
            })
            .collect())
    }
}

fn files_is_empty(file_list_literal: &str) -> bool {
    file_list_literal == "[]"
}
