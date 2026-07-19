//! The `GraphStore` trait implementation for `FalkorStore` — the domain query
//! surface (context, impact, flow traversal, route map, communities, …) plus the
//! load/publish lifecycle. Cypher is built with the `serialize` helpers; the
//! `FalkorStore` connection/exec primitives live in `lib.rs`.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use cih_core::{Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId, NodeKind, Range};
use cih_graph_store::{
    risk_from_fanout, CallSiteArgs, CommunityEdge, CommunityInfo, Direction, FlowEdge, FlowHop,
    FlowNode, GraphOverview, GraphOverviewEdge, GraphOverviewNode, GraphStore, GraphStoreError,
    GraphSummary, HotspotNode, Impact, ImpactNode, InterceptingAdvice, KindCount, LoadObserver,
    LoadStats, NoopObserver, Path, Result, RouteInfo, SimilarMethod, Subgraph, SymbolContext,
};

use crate::neighbor_nodes;
use crate::serialize::*;
use crate::FalkorStore;

#[async_trait]
impl GraphStore for FalkorStore {
    async fn ensure_schema(&self) -> Result<()> {
        let _ = self.run("CREATE INDEX FOR (n:Symbol) ON (n.id)").await;
        let _ = self.run("CREATE INDEX FOR (n:Symbol) ON (n.kind)").await;
        // Short-name symbol resolution (`candidates_by_name`) filters on n.name on
        // nearly every context/impact/trace_flow call; without this index it is a
        // full label scan over all Symbol nodes.
        let _ = self.run("CREATE INDEX FOR (n:Symbol) ON (n.name)").await;
        Ok(())
    }

    async fn bulk_load(&self, artifacts: &GraphArtifacts) -> Result<LoadStats> {
        self.bulk_load_observed(artifacts, &NoopObserver).await
    }

    /// `bulk_load` with a progress observer; a CLI passes an observer that
    /// renders per-phase progress. Routing: a fresh (unused) key takes the
    /// native `GRAPH.BULK` fast path; a populated one falls back to the Cypher
    /// upsert.
    async fn bulk_load_observed(
        &self,
        artifacts: &GraphArtifacts,
        obs: &dyn LoadObserver,
    ) -> Result<LoadStats> {
        // Wait out a `BusyLoadingError` window before writing, so a FalkorDB that
        // is still loading a large persisted dataset doesn't cause a partial write.
        self.wait_until_ready(Self::load_wait_budget()).await?;
        // A fresh (unused) graph key takes the native `GRAPH.BULK` fast path,
        // which streams the artifacts (no `Vec` read). A populated one (e.g. the
        // community set loaded after analyze) falls back to the Cypher upsert,
        // which reads the small set into memory. `GRAPH.BULK BEGIN` also requires
        // an unused key, so this routing honors that constraint.
        if self.graph_is_empty().await? {
            self.bulk_insert(artifacts, obs).await
        } else {
            let nodes = artifacts
                .read_nodes()
                .map_err(|e| GraphStoreError::Backend(format!("read nodes: {e}")))?;
            let edges = artifacts
                .read_edges()
                .map_err(|e| GraphStoreError::Backend(format!("read edges: {e}")))?;
            self.load_nodes_edges(&nodes, &edges, obs).await
        }
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
        self.load_nodes_edges(&delta.nodes, &delta.edges, &NoopObserver)
            .await?;
        Ok(())
    }

    async fn publish_to(&self, dest_key: &str) -> Result<()> {
        // Redis RENAME is O(1), atomic, and fork-free — the kernel does not need to
        // duplicate FalkorDB's RSS to execute it. GRAPH.COPY forks the process, which
        // fails on memory-constrained hosts when the graph is large (> ~4 GB RSS).
        // If dest_key already exists Redis atomically replaces it, so there is no
        // window where the live graph is absent.
        self.graph_command("RENAME", &[&self.graph_key, dest_key])
            .await?;
        Ok(())
    }

    async fn drop_graph(&self) -> Result<()> {
        match self.graph_command("GRAPH.DELETE", &[&self.graph_key]).await {
            Ok(_) => Ok(()),
            // GRAPH.DELETE on a nonexistent key errors "Invalid graph operation on
            // empty key". Dropping an absent graph is a no-op success — this makes
            // `drop_graph` idempotent (e.g. after `publish_to` RENAMEs staging away).
            Err(GraphStoreError::Backend(msg)) if msg.contains("empty key") => Ok(()),
            Err(e) => Err(e),
        }
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
        // startNode/endNode preserve the STORED edge direction — the contract
        // suite asserts `src`/`dst` reflect the graph, not the query pattern
        // (an Upstream query must still report the caller as `src`).
        let q = format!(
            "CYPHER id={id} MATCH {pat} RETURN type(r), startNode(r).id, endNode(r).id",
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
                props: None,
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

    async fn graph_summary(&self) -> Result<GraphSummary> {
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
        let kinds = self
            .rows("MATCH (n:Symbol) RETURN n.kind, count(n) ORDER BY count(n) DESC")
            .await?
            .into_iter()
            .filter_map(|row| {
                if row.len() < 2 {
                    return None;
                }
                let count = row[1].parse::<u64>().ok()?;
                Some(KindCount {
                    kind: row[0].clone(),
                    count,
                })
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

        let mut internal_to_node = HashMap::<i64, NodeId>::with_capacity(max_nodes);
        let mut nodes = Vec::with_capacity(max_nodes.min(total_nodes as usize));

        if let Some(kind_list) = kinds {
            // User-selected kinds: single filtered query with degree scan only on the subset.
            // Use cstr() for every value to prevent Cypher injection from crafted kind strings.
            let kind_literals = kind_list
                .iter()
                .map(|k| cstr(k))
                .collect::<Vec<_>>()
                .join(",");
            let node_query = format!(
                "MATCH (n:Symbol) \
                 WHERE n.kind IN [{kind_literals}] \
                 OPTIONAL MATCH (n)-[r]-(:Symbol) \
                 WITH n, count(r) AS degree \
                 ORDER BY degree DESC, n.id ASC \
                 LIMIT {max_nodes} \
                 RETURN id(n), n.id, n.kind, n.name, n.qualifiedName, n.file, degree"
            );
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
                        qualified_name: row.get(4).filter(|v| !v.is_empty()).cloned(),
                        file: row.get(5).cloned().unwrap_or_default(),
                        range: Range::default(),
                        props: None,
                    },
                    degree: row[6].parse::<u64>().unwrap_or(0),
                });
            }
        } else {
            // No filter: two-pass to avoid full-graph degree scan.
            // Pass 1: architectural/runtime nodes — shown regardless of degree.
            let structural_kinds = "['Community','Process','Route','IntegrationRoute',\
                  'MessageDestination','KafkaTopic','ExternalEndpoint',\
                  'DbTable','DbQuery']";
            let pass1_limit = max_nodes.min(2_000);
            let pass1_query = format!(
                "MATCH (n:Symbol) \
                 WHERE n.kind IN {structural_kinds} \
                 RETURN id(n), n.id, n.kind, n.name, n.qualifiedName, n.file \
                 LIMIT {pass1_limit}"
            );
            for row in self.rows(&pass1_query).await? {
                if row.len() < 6 {
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
                        qualified_name: row.get(4).filter(|v| !v.is_empty()).cloned(),
                        file: row.get(5).cloned().unwrap_or_default(),
                        range: Range::default(),
                        props: None,
                    },
                    degree: 0,
                });
            }

            // Pass 2: fill remaining budget with Class-family nodes ordered by degree.
            let remaining = max_nodes.saturating_sub(nodes.len());
            if remaining > 0 {
                let class_kinds = "['Class','Interface','Enum','Record']";
                let pass2_query = format!(
                    "MATCH (n:Symbol) \
                     WHERE n.kind IN {class_kinds} \
                     OPTIONAL MATCH (n)-[r]-(:Symbol) \
                     WITH n, count(r) AS degree \
                     ORDER BY degree DESC, n.id ASC \
                     LIMIT {remaining} \
                     RETURN id(n), n.id, n.kind, n.name, n.qualifiedName, n.file, degree"
                );
                for row in self.rows(&pass2_query).await? {
                    if row.len() < 7 {
                        continue;
                    }
                    let Ok(internal_id) = row[0].parse::<i64>() else {
                        continue;
                    };
                    if internal_to_node.contains_key(&internal_id) {
                        continue;
                    }
                    let id = NodeId::new(row[1].clone());
                    internal_to_node.insert(internal_id, id.clone());
                    nodes.push(GraphOverviewNode {
                        node: Node {
                            id,
                            kind: NodeKind::from_label(&row[2]),
                            name: row[3].clone(),
                            qualified_name: row.get(4).filter(|v| !v.is_empty()).cloned(),
                            file: row.get(5).cloned().unwrap_or_default(),
                            range: Range::default(),
                            props: None,
                        },
                        degree: row[6].parse::<u64>().unwrap_or(0),
                    });
                }
            }
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
        let proc_query = format!(
            "CYPHER id={id} \
             MATCH (s:Symbol {{id:$id}})-[:STEP_IN_PROCESS]->(p:Symbol) \
             WHERE p.kind = 'Process' \
             RETURN p.id ORDER BY p.name",
            id = cstr(id.as_str())
        );
        // callers / callees / processes / community are independent of one another;
        // fire them concurrently over the multiplexed connection instead of paying
        // four sequential round-trips.
        let processes_fut = async {
            Ok::<Vec<String>, GraphStoreError>(
                self.rows(&proc_query)
                    .await?
                    .into_iter()
                    .filter_map(|row| row.first().cloned())
                    .collect(),
            )
        };
        let community_fut = async {
            Ok::<Option<CommunityInfo>, GraphStoreError>(
                self.symbol_communities(std::slice::from_ref(id))
                    .await?
                    .into_iter()
                    .find_map(|(nid, info)| if &nid == id { Some(info) } else { None }),
            )
        };
        let (callers, callees, processes, community) = tokio::try_join!(
            neighbor_nodes(self, id, Direction::Upstream),
            neighbor_nodes(self, id, Direction::Downstream),
            processes_fut,
            community_fut,
        )?;
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

    async fn flow_downstream(&self, entry: &NodeId, max_depth: u32) -> Result<Vec<FlowHop>> {
        let d = max_depth.clamp(1, 10);

        // A Route node has no *outgoing* flow edges — `HANDLES_ROUTE` is stored
        // handler→route. When the entry is a route, hop route→handler via the
        // inverse `HANDLES_ROUTE` first, then trace downstream from each handler
        // (recursion terminates: a handler is a method, so it has no inverse
        // handlers and falls through to the plain method walk below).
        let handlers = self.route_handler_nodes(entry).await?;
        if !handlers.is_empty() {
            // Collect each handler with its downstream walk, then assemble the
            // route-entry flow. Recursion terminates: a handler is a method, so
            // it has no inverse handlers and falls through to the method walk.
            let mut with_downstream = Vec::with_capacity(handlers.len());
            for handler in handlers {
                let sub = self.flow_downstream(&handler.id, d).await?;
                with_downstream.push((handler, sub));
            }
            let mut hops = assemble_route_flow(entry, with_downstream);
            self.annotate_interceptions(&mut hops).await?;
            return Ok(hops);
        }

        // Phase 1: BFS to get node order, depth, and parent relationships.
        let q = format!(
            "CYPHER id={id} \
             MATCH p=(start:Symbol {{id:$id}})\
             -[:CALLS|HANDLES_ROUTE|EXTERNAL_CALL|PUBLISHES_EVENT|LISTENS_TO*1..{d}]->(m:Symbol) \
             WITH m, length(p) AS len, nodes(p)[length(p)-1] AS pnode, \
                  type(relationships(p)[length(p)-1]) AS etype \
             ORDER BY m.id, len \
             WITH m, collect(pnode)[0] AS parent, collect(etype)[0] AS etype, min(len) AS depth \
             RETURN m.id, m.kind, m.name, m.qualifiedName, m.file, depth, parent.id, etype \
             ORDER BY depth, m.name LIMIT 100",
            id = cstr(entry.as_str())
        );
        let rows = self.rows(&q).await?;
        // Build FlowNode list and collect (parent_id, child_id) pairs.
        let mut flow_nodes: Vec<FlowNode> = rows
            .iter()
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
                intercepted_by: Vec::new(),
            })
            .collect();

        // The relationship type of each node's incoming (min-depth) edge, so the
        // trace labels the real hop kind (EXTERNAL_CALL / PUBLISHES_EVENT / …)
        // instead of a blanket "CALLS".
        let edge_kind: HashMap<String, String> = rows
            .iter()
            .filter_map(|r| {
                let etype = r.get(7).filter(|s| !s.is_empty())?;
                Some((r[0].clone(), etype.clone()))
            })
            .collect();

        // Phase 2: for each (parent, child) pair, fetch the CALLS edge callSites.
        // We do a single batch query returning (src.id, dst.id, r.callSites).
        let mut call_sites_map: HashMap<(String, String), Vec<CallSiteArgs>> = HashMap::new();
        if !flow_nodes.is_empty() {
            // Collect unique (parent_id, child_id) pairs that have a parent.
            let pairs: Vec<(String, String)> = flow_nodes
                .iter()
                .filter_map(|n| {
                    n.parent_id
                        .as_ref()
                        .map(|p| (p.as_str().to_string(), n.id.as_str().to_string()))
                })
                .collect();
            if !pairs.is_empty() {
                let pair_list = pairs
                    .iter()
                    .map(|(s, d)| format!("[{}, {}]", cstr(s), cstr(d)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let eq = format!(
                    "UNWIND [{pairs}] AS pair \
                     MATCH (a:Symbol {{id: pair[0]}})-[r:CALLS]->(b:Symbol {{id: pair[1]}}) \
                     RETURN a.id, b.id, r.callSites",
                    pairs = pair_list
                );
                if let Ok(edge_rows) = self.rows(&eq).await {
                    for row in edge_rows.iter().filter(|r| r.len() >= 3) {
                        let src_id = row[0].clone();
                        let dst_id = row[1].clone();
                        let cs_json = row[2].as_str();
                        // callSites is stored as a JSON string
                        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(cs_json) {
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
                            call_sites_map.insert((src_id, dst_id), call_sites);
                        }
                    }
                }
            }
        }

        // Build root hop (no via edge).
        let entry_node = FlowNode {
            id: entry.clone(),
            kind: NodeKind::Method,
            name: entry.as_str().rsplit('#').next().unwrap_or("").to_string(),
            qualified_name: None,
            file: String::new(),
            depth: 0,
            parent_id: None,
            intercepted_by: Vec::new(),
        };
        let mut hops: Vec<FlowHop> = vec![FlowHop {
            node: entry_node,
            via: None,
        }];
        for node in flow_nodes.drain(..) {
            let via = if let Some(ref parent_id) = node.parent_id {
                let key = (parent_id.as_str().to_string(), node.id.as_str().to_string());
                let call_sites = call_sites_map.remove(&key).unwrap_or_default();
                let kind = edge_kind
                    .get(node.id.as_str())
                    .cloned()
                    .unwrap_or_else(|| "CALLS".to_string());
                Some(FlowEdge { kind, call_sites })
            } else {
                None
            };
            hops.push(FlowHop { node, via });
        }
        self.annotate_interceptions(&mut hops).await?;
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
        Ok(self
            .rows(&q)
            .await?
            .into_iter()
            .filter(|r| r.len() >= 6)
            .map(|r| HotspotNode {
                id: NodeId::new(r[0].clone()),
                name: r[1].clone(),
                file: r[2].clone(),
                cyclomatic: r[3].parse().unwrap_or(0),
                cognitive: r[4].parse().unwrap_or(0),
                transitive_loop_depth: r[5].parse().unwrap_or(0),
            })
            .collect())
    }

    async fn similar_methods(
        &self,
        id: &NodeId,
        _min_jaccard: f32,
        limit: usize,
    ) -> Result<Vec<SimilarMethod>> {
        let _id_lit = cstr(id.as_str());
        let lim = limit.clamp(1, 50) as i64;
        // SIMILAR_TO edges carry confidence = Jaccard score.
        let q = format!(
            "CYPHER id={id_lit} \
             MATCH (a:Symbol {{id:$id}})-[r:SIMILAR_TO]->(b:Symbol) \
             RETURN b.id, b.name, b.file, r.confidence \
             ORDER BY r.confidence DESC LIMIT {lim}",
            id_lit = cstr(id.as_str())
        );
        Ok(self
            .rows(&q)
            .await?
            .into_iter()
            .filter(|r| r.len() >= 4)
            .map(|r| SimilarMethod {
                id: NodeId::new(r[0].clone()),
                name: r[1].clone(),
                file: r[2].clone(),
                jaccard: r[3].parse().unwrap_or(0.0),
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
        // Direct TESTS edges to this symbol, plus TESTS edges to its owner
        // class. Pattern predicate, not `EXISTS { MATCH … }` — FalkorDB
        // rejects the braced-subquery form (contract-tested).
        let q = format!(
            "MATCH (t:Symbol)-[:TESTS]->(target:Symbol) \
             WHERE target.id = {id_lit} \
                OR (target)-[:HAS_METHOD]->(:Symbol {{id: {id_lit}}}) \
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
        // Two fixes over the original (both contract-tested): pattern
        // predicate instead of the rejected `EXISTS { MATCH … }` form, and
        // explicit NULL handling — `NOT n.stereotype = 'test'` is three-valued
        // NULL for nodes without a stereotype, which silently excluded every
        // unannotated symbol (i.e. almost all of them).
        let q = format!(
            "MATCH (n:Symbol) \
             WHERE n.file STARTS WITH {prefix_lit} \
               AND n.kind IN ['Method', 'Class', 'Interface'] \
               AND (n.stereotype IS NULL OR n.stereotype <> 'test') \
               AND NOT (:Symbol)-[:TESTS]->(n) \
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

impl FalkorStore {
    /// Attach `intercepted_by` to traced hops: one batched query for `ADVISES`
    /// edges into the hop methods. Advice wraps calls through the Spring proxy
    /// — it is not a call-graph hop, so it annotates nodes rather than
    /// extending the path. Idempotent: hops already annotated (route entries
    /// assembled from recursively traced handlers) are left untouched.
    pub(crate) async fn annotate_interceptions(&self, hops: &mut [FlowHop]) -> Result<()> {
        let ids: Vec<&str> = hops
            .iter()
            .filter(|h| {
                h.node.intercepted_by.is_empty()
                    && matches!(h.node.kind, NodeKind::Method | NodeKind::Function)
            })
            .map(|h| h.node.id.as_str())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let list = ids.iter().map(|s| cstr(s)).collect::<Vec<_>>().join(", ");
        let q = format!(
            "UNWIND [{list}] AS mid \
             MATCH (a:Symbol)-[r:ADVISES]->(m:Symbol {{id: mid}}) \
             RETURN m.id, a.id, r.reason"
        );
        let mut by_target: HashMap<String, Vec<InterceptingAdvice>> = HashMap::new();
        for row in self.rows(&q).await? {
            if row.len() < 3 {
                continue;
            }
            let kind = row[2].strip_prefix("aop-").unwrap_or(&row[2]).to_string();
            by_target
                .entry(row[0].clone())
                .or_default()
                .push(InterceptingAdvice {
                    advice: NodeId::new(row[1].clone()),
                    advice_kind: kind,
                });
        }
        if by_target.is_empty() {
            return Ok(());
        }
        for hop in hops.iter_mut() {
            if let Some(advices) = by_target.get(hop.node.id.as_str()) {
                if hop.node.intercepted_by.is_empty() {
                    hop.node.intercepted_by = advices.clone();
                }
            }
        }
        Ok(())
    }
}

/// Assemble a route-entry flow: route at depth 0, handlers at depth 1 via
/// HANDLES_ROUTE, downstream shifted one level, deduped by id, capped at 100.
pub(crate) fn assemble_route_flow(
    entry: &NodeId,
    handlers: Vec<(FlowNode, Vec<FlowHop>)>,
) -> Vec<FlowHop> {
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
            intercepted_by: Vec::new(),
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
        // Drop the handler's own root hop (index 0 — already emitted as the
        // depth-1 handler above) and shift the rest one level past the route.
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
