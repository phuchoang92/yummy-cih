//! The `GraphStore` trait implementation for `FalkorStore` — the domain query
//! surface (context, impact, flow traversal, route map, communities, …) plus the
//! load/publish lifecycle. Cypher is built with the `serialize` helpers; the
//! `FalkorStore` connection/exec primitives live in `lib.rs`.

use std::cmp::Ordering;
use std::collections::HashMap;

use async_trait::async_trait;
use cih_core::{Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId};
use cih_graph_store::{
    stored_edge_token, BackendReadiness, CallSiteArgs, CommunityEdge, CommunityInfo, ContextFilter,
    ContextPage, ContextSection, DbEffect, Direction, ExecutionTransition, GraphOverview,
    GraphOverviewEdge, GraphOverviewNode, GraphStore, GraphStoreError, GraphSummary, HotspotNode,
    InterceptingAdvice, Interception, KindCount, LoadObserver, LoadStats, NoopObserver, Path,
    Result, RouteInfo, SimilarMethod, StoredTransition, SymbolContext, TransitionBatch,
    TransitionQuery, EXECUTION_BATCH_SIZE,
};

use crate::neighbor_nodes;
use crate::serialize::*;
use crate::{FalkorStore, WRITE_LOAD_WAIT_SETTING};

#[async_trait]
impl GraphStore for FalkorStore {
    async fn ensure_schema(&self) -> Result<()> {
        self.ensure_required_symbol_indexes().await
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
        self.wait_until_ready(Self::load_wait_budget(), WRITE_LOAD_WAIT_SETTING)
            .await?;
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

    async fn backend_readiness(&self) -> Result<BackendReadiness> {
        self.probe_backend_readiness().await
    }

    async fn get_node(&self, id: &NodeId) -> Result<Option<Node>> {
        let columns = node_columns("n");
        let q = format!(
            "CYPHER id={id} \
             MATCH (n:Symbol {{id:$id}}) \
             RETURN {columns} LIMIT 1",
            id = cstr(id.as_str())
        );
        let rows = self.rows(&q).await?;
        Ok(rows.first().map(|r| node_from_row(r)))
    }

    async fn batched_transitions(
        &self,
        sources: &[NodeId],
        query: &TransitionQuery,
    ) -> Result<TransitionBatch> {
        query.validate(sources.len())?;
        if sources.is_empty() {
            return Ok(TransitionBatch {
                transitions: Vec::new(),
                next_cursor: None,
                backend_limited: false,
            });
        }
        let list = sources
            .iter()
            .map(|id| cstr(id.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let rel = rel_filter(&query.edge_kinds);
        let columns = node_columns("t");
        let probe_limit = query.page_limit + 1;
        let mut transitions = Vec::new();
        if matches!(query.direction, Direction::Downstream | Direction::Both) {
            let cursor = transition_cursor_predicate(query.after.as_ref(), false, "type(r)")
                .map_or_else(String::new, |predicate| format!("WHERE {predicate} "));
            let q = format!(
                "UNWIND [{list}] AS sid \
                 MATCH (s:Symbol {{id:sid}})-[r{rel}]->(t:Symbol) \
                 {cursor}RETURN s.id, {columns}, type(r), \
                        coalesce(r.confidence, 1.0), coalesce(r.reason, ''), \
                        coalesce(r.callSites, '') \
                 ORDER BY s.id, coalesce(t.name, ''), t.id, type(r) LIMIT {probe_limit}"
            );
            transitions.extend(parse_stored_rows(self.rows(&q).await?, false));
        }
        if matches!(query.direction, Direction::Upstream | Direction::Both) {
            let cursor = transition_cursor_predicate(query.after.as_ref(), true, "type(r)")
                .map_or_else(String::new, |predicate| format!("WHERE {predicate} "));
            let q = format!(
                "UNWIND [{list}] AS sid \
                 MATCH (s:Symbol {{id:sid}})<-[r{rel}]-(t:Symbol) \
                 {cursor}RETURN s.id, {columns}, type(r), \
                        coalesce(r.confidence, 1.0), coalesce(r.reason, ''), \
                        coalesce(r.callSites, '') \
                 ORDER BY s.id, coalesce(t.name, ''), t.id, type(r) LIMIT {probe_limit}"
            );
            transitions.extend(parse_stored_rows(self.rows(&q).await?, true));
        }
        transitions.sort_by(stored_transition_order);
        transitions.dedup_by(|a, b| a.cursor_key() == b.cursor_key());
        let backend_limited = transitions.len() > query.page_limit;
        transitions.truncate(query.page_limit);
        let next_cursor = backend_limited
            .then(|| transitions.last().map(StoredTransition::cursor_key))
            .flatten();
        Ok(TransitionBatch {
            transitions,
            next_cursor,
            backend_limited,
        })
    }

    async fn execution_transitions(
        &self,
        ids: &[NodeId],
        include_data: bool,
        limit: usize,
    ) -> Result<Vec<ExecutionTransition>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        if ids.len() > EXECUTION_BATCH_SIZE {
            return Err(GraphStoreError::InvalidInput(format!(
                "execution transition batch {} exceeds {EXECUTION_BATCH_SIZE}",
                ids.len()
            )));
        }
        let list = ids
            .iter()
            .map(|id| cstr(id.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        let outgoing = if include_data {
            "CALLS|EXTERNAL_CALL|PUBLISHES_EVENT|EXECUTES_QUERY|READS_TABLE|WRITES_TABLE"
        } else {
            "CALLS|EXTERNAL_CALL|PUBLISHES_EVENT"
        };
        let limit = limit.clamp(1, 50_001);
        let target_columns = node_columns("t");
        let forward = format!(
            "UNWIND [{list}] AS sid \
             MATCH (s:Symbol {{id:sid}})-[r:{outgoing}]->(t:Symbol) \
             RETURN s.id, {target_columns}, coalesce(t.isAccessor, 0), \
                    type(r), coalesce(r.confidence, 1.0), coalesce(r.reason, ''), \
                    coalesce(r.callSites, '') \
             ORDER BY t.name, t.id, s.id, type(r) LIMIT {limit}"
        );
        let reverse = format!(
            "UNWIND [{list}] AS sid \
             MATCH (s:Symbol {{id:sid}})<-[r:HANDLES_ROUTE|LISTENS_TO]-(t:Symbol) \
             RETURN s.id, {target_columns}, coalesce(t.isAccessor, 0), \
                    type(r), coalesce(r.confidence, 1.0), coalesce(r.reason, ''), \
                    coalesce(r.callSites, '') \
             ORDER BY t.name, t.id, s.id, type(r) LIMIT {limit}"
        );
        let mut transitions = parse_execution_rows(self.rows(&forward).await?, false);
        transitions.extend(parse_execution_rows(self.rows(&reverse).await?, true));
        Ok(transitions)
    }

    async fn interceptions_for_methods(&self, ids: &[NodeId]) -> Result<Vec<Interception>> {
        let mut matches = Vec::new();
        for chunk in ids.chunks(EXECUTION_BATCH_SIZE) {
            let list = chunk
                .iter()
                .map(|id| cstr(id.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            if list.is_empty() {
                continue;
            }
            let q = format!(
                "UNWIND [{list}] AS mid \
                 MATCH (a:Symbol)-[r:ADVISES]->(m:Symbol {{id: mid}}) \
                 RETURN m.id, a.id, r.reason"
            );
            matches.extend(self.rows(&q).await?.into_iter().filter_map(|row| {
                if row.len() < 3 {
                    return None;
                }
                let advice_kind = row[2].strip_prefix("aop-").unwrap_or(&row[2]).to_string();
                Some(Interception {
                    target: NodeId::new(row[0].clone()),
                    advice: InterceptingAdvice {
                        advice: NodeId::new(row[1].clone()),
                        advice_kind,
                    },
                })
            }));
        }
        Ok(matches)
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
            let columns = node_columns("n");
            let node_query = format!(
                "MATCH (n:Symbol) \
                 WHERE n.kind IN [{kind_literals}] \
                 OPTIONAL MATCH (n)-[r]-(:Symbol) \
                 WITH n, count(r) AS degree \
                 ORDER BY degree DESC, n.id ASC \
                 LIMIT {max_nodes} \
                 RETURN id(n), {columns}, degree"
            );
            for row in self.rows(&node_query).await? {
                if row.len() < 9 {
                    continue;
                }
                let Ok(internal_id) = row[0].parse::<i64>() else {
                    continue;
                };
                let node = node_from_row(&row[1..8]);
                internal_to_node.insert(internal_id, node.id.clone());
                nodes.push(GraphOverviewNode {
                    node,
                    degree: row[8].parse::<u64>().unwrap_or(0),
                });
            }
        } else {
            // No filter: two-pass to avoid full-graph degree scan.
            // Pass 1: architectural/runtime nodes — shown regardless of degree.
            let structural_kinds = "['Community','Process','Route','IntegrationRoute',\
                  'MessageDestination','KafkaTopic','ExternalEndpoint',\
                  'DbTable','DbQuery']";
            let pass1_limit = max_nodes.min(2_000);
            let columns = node_columns("n");
            let pass1_query = format!(
                "MATCH (n:Symbol) \
                 WHERE n.kind IN {structural_kinds} \
                 RETURN id(n), {columns} \
                 LIMIT {pass1_limit}"
            );
            for row in self.rows(&pass1_query).await? {
                if row.len() < 8 {
                    continue;
                }
                let Ok(internal_id) = row[0].parse::<i64>() else {
                    continue;
                };
                let node = node_from_row(&row[1..8]);
                internal_to_node.insert(internal_id, node.id.clone());
                nodes.push(GraphOverviewNode { node, degree: 0 });
            }

            // Pass 2: fill remaining budget with Class-family nodes ordered by degree.
            let remaining = max_nodes.saturating_sub(nodes.len());
            if remaining > 0 {
                let class_kinds = "['Class','Interface','Enum','Record']";
                let columns = node_columns("n");
                let pass2_query = format!(
                    "MATCH (n:Symbol) \
                     WHERE n.kind IN {class_kinds} \
                     OPTIONAL MATCH (n)-[r]-(:Symbol) \
                     WITH n, count(r) AS degree \
                     ORDER BY degree DESC, n.id ASC \
                     LIMIT {remaining} \
                     RETURN id(n), {columns}, degree"
                );
                for row in self.rows(&pass2_query).await? {
                    if row.len() < 9 {
                        continue;
                    }
                    let Ok(internal_id) = row[0].parse::<i64>() else {
                        continue;
                    };
                    if internal_to_node.contains_key(&internal_id) {
                        continue;
                    }
                    let node = node_from_row(&row[1..8]);
                    internal_to_node.insert(internal_id, node.id.clone());
                    nodes.push(GraphOverviewNode {
                        node,
                        degree: row[8].parse::<u64>().unwrap_or(0),
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
        let page = self.context_page(id, &ContextFilter::default()).await?;
        if page.callers.has_more || page.callees.has_more || page.processes.has_more {
            return Err(GraphStoreError::InvalidInput(
                "context exceeds the exact legacy 100-item section cap; use context_page"
                    .to_string(),
            ));
        }
        Ok(SymbolContext {
            node: page.node,
            callers: page.callers.items,
            callees: page.callees.items,
            processes: page.processes.items,
            community: page.community,
        })
    }

    async fn context_page(&self, id: &NodeId, filter: &ContextFilter) -> Result<ContextPage> {
        filter.validate()?;
        let node = self
            .get_node(id)
            .await?
            .ok_or_else(|| GraphStoreError::NotFound(id.to_string()))?;
        let (process_params, process_predicate) = filter.process_after.as_ref().map_or_else(
            || (String::new(), String::new()),
            |after| {
                (
                    format!(
                        " after_name={} after_id={}",
                        cstr(&after.name),
                        cstr(&after.id)
                    ),
                    "AND (p.name > $after_name OR (p.name = $after_name AND p.id > $after_id)) "
                        .to_string(),
                )
            },
        );
        let process_probe = filter.process_limit + 1;
        let proc_query = format!(
            "CYPHER id={id}{process_params} \
             MATCH (s:Symbol {{id:$id}})-[:STEP_IN_PROCESS]->(p:Symbol) \
             WHERE p.kind = 'Process' {process_predicate}\
             RETURN DISTINCT p.name, p.id \
             ORDER BY p.name, p.id LIMIT {process_probe}",
            id = cstr(id.as_str())
        );
        // callers / callees / processes / community are independent of one another;
        // fire them concurrently over the multiplexed connection instead of paying
        // four sequential round-trips.
        let processes_fut = async {
            let processes = self
                .rows(&proc_query)
                .await?
                .into_iter()
                .filter(|row| row.len() >= 2)
                .map(|row| (row[0].clone(), row[1].clone()))
                .collect();
            Ok::<ContextSection<String>, GraphStoreError>(ContextSection::from_process_probe(
                processes,
                filter.process_limit,
                filter.process_after.as_ref(),
            ))
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
            neighbor_nodes(
                self,
                id,
                Direction::Upstream,
                filter.caller_limit,
                filter.caller_after.as_ref(),
            ),
            neighbor_nodes(
                self,
                id,
                Direction::Downstream,
                filter.callee_limit,
                filter.callee_after.as_ref(),
            ),
            processes_fut,
            community_fut,
        )?;
        Ok(ContextPage {
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
        let columns = node_columns("n");
        let q = format!(
            "CYPHER name={name_lit} \
             MATCH (n:Symbol) WHERE n.name = $name \
             RETURN {columns} \
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

    async fn nodes_in_files(&self, files: &[String], limit: usize) -> Result<Vec<Node>> {
        if files.is_empty() {
            return Ok(vec![]);
        }
        let list = format!(
            "[{}]",
            files.iter().map(|f| cstr(f)).collect::<Vec<_>>().join(", ")
        );
        // Limit to callable/structural kinds most useful for change-impact analysis.
        // The bounded `LIMIT` keeps a single huge generated file from loading an
        // unbounded symbol set into memory; the deterministic order makes the
        // truncated page stable.
        let columns = node_columns("n");
        let q = format!(
            "MATCH (n:Symbol) \
             WHERE n.file IN {list} \
               AND n.kind IN ['Method', 'Constructor', 'Function', 'Class', 'Interface', 'Enum'] \
             RETURN {columns} \
             ORDER BY n.file, n.id \
             LIMIT {limit}"
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

    async fn db_effects_for_methods(&self, ids: &[NodeId]) -> Result<Vec<DbEffect>> {
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
        // `operation`/`sqlPreview` live inside the DbQuery's serialized `props`
        // JSON (they are not promoted graph properties) — parse client-side.
        let q = format!(
            "MATCH (m:Symbol)-[:EXECUTES_QUERY]->(q:Symbol)-[r:READS_TABLE|WRITES_TABLE]->(t:Symbol) \
             WHERE m.id IN {list} \
             RETURN m.id, q.id, coalesce(q.props, ''), t.name, type(r) \
             ORDER BY m.id, t.name"
        );
        Ok(self
            .rows(&q)
            .await?
            .into_iter()
            .filter_map(|row| {
                let mut it = row.into_iter();
                let method = NodeId::new(it.next()?);
                let query = NodeId::new(it.next()?);
                let props = it.next()?;
                let table = it.next()?;
                let rel = it.next()?;
                Some(DbEffect::from_query_row(method, query, &props, table, &rel))
            })
            .collect())
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
        let columns = node_columns("t");
        // Direct TESTS edges to this symbol, plus TESTS edges to its owner
        // class. Pattern predicate, not `EXISTS { MATCH … }` — FalkorDB
        // rejects the braced-subquery form (contract-tested).
        let q = format!(
            "MATCH (t:Symbol)-[:TESTS]->(target:Symbol) \
             WHERE target.id = {id_lit} \
                OR (target)-[:HAS_METHOD]->(:Symbol {{id: {id_lit}}}) \
             RETURN DISTINCT {columns} \
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
        let columns = node_columns("t");
        let q = format!(
            "MATCH (t:Symbol)-[:TESTS]->(prod:Symbol) \
             WHERE prod.file IN {list} \
             RETURN DISTINCT {columns} \
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
             RETURN DISTINCT {columns} \
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
        let columns = node_columns("n");
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
             RETURN {columns} \
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

fn parse_execution_rows(
    rows: Vec<Vec<String>>,
    traversed_reverse: bool,
) -> Vec<ExecutionTransition> {
    rows.into_iter()
        .filter_map(|row| {
            if row.len() < 13 {
                return None;
            }
            Some(ExecutionTransition {
                source: NodeId::new(row[0].clone()),
                target: node_from_row(&row[1..8]),
                target_is_accessor: row[8] == "1" || row[8].eq_ignore_ascii_case("true"),
                kind: row[9].clone(),
                confidence: row[10].parse().unwrap_or(1.0),
                reason: row[11].clone(),
                call_sites: parse_call_sites(&row[12]),
                traversed_reverse,
            })
        })
        .collect()
}

fn transition_cursor_predicate(
    after: Option<&cih_graph_store::TransitionCursorKey>,
    traversed_reverse: bool,
    edge_kind_expression: &str,
) -> Option<String> {
    let after = after?;
    let source = cstr(after.source_id.as_str());
    let target_name = cstr(&after.target_name);
    let target_id = cstr(after.target_id.as_str());
    let edge_kind = cstr(&after.edge_kind);
    let reverse = usize::from(traversed_reverse);
    let after_reverse = usize::from(after.traversed_reverse);
    Some(format!(
        "s.id > {source} OR (s.id = {source} AND (\
         coalesce(t.name, '') > {target_name} OR \
         (coalesce(t.name, '') = {target_name} AND (\
          t.id > {target_id} OR (t.id = {target_id} AND (\
           {edge_kind_expression} > {edge_kind} OR \
           ({edge_kind_expression} = {edge_kind} AND {reverse} > {after_reverse})\
          ))\
         ))\
        ))"
    ))
}

fn parse_stored_rows(rows: Vec<Vec<String>>, traversed_reverse: bool) -> Vec<StoredTransition> {
    rows.into_iter()
        .filter_map(|row| {
            if row.len() < 12 {
                return None;
            }
            let source = NodeId::new(row[0].clone());
            let target = node_from_row(&row[1..8]);
            let kind = edge_from_label(&row[8]);
            let (stored_src, stored_dst) = if traversed_reverse {
                (target.id.clone(), source.clone())
            } else {
                (source.clone(), target.id.clone())
            };
            let call_sites = serde_json::from_str::<serde_json::Value>(&row[11])
                .ok()
                .filter(|value| value.as_array().is_some_and(|items| !items.is_empty()))
                .map(|value| serde_json::json!({"call_sites": value}));
            let edge = Edge {
                src: stored_src.clone(),
                dst: stored_dst.clone(),
                kind,
                confidence: row[9].parse().unwrap_or(1.0),
                reason: row[10].clone(),
                props: call_sites,
            };
            Some(StoredTransition {
                source,
                target,
                stored_edge_token: stored_edge_token(&stored_src, &stored_dst, kind),
                edge,
                traversed_reverse,
            })
        })
        .collect()
}

fn stored_transition_order(a: &StoredTransition, b: &StoredTransition) -> Ordering {
    a.source
        .as_str()
        .cmp(b.source.as_str())
        .then_with(|| a.target.name.cmp(&b.target.name))
        .then_with(|| a.target.id.as_str().cmp(b.target.id.as_str()))
        .then_with(|| a.edge.kind.cypher_label().cmp(b.edge.kind.cypher_label()))
        .then_with(|| a.traversed_reverse.cmp(&b.traversed_reverse))
        .then_with(|| a.stored_edge_token.cmp(&b.stored_edge_token))
}

fn parse_call_sites(raw: &str) -> Vec<CallSiteArgs> {
    let values: Vec<serde_json::Value> = match serde_json::from_str(raw) {
        Ok(values) => values,
        Err(error) => {
            // Empty / null / [] is a legitimately-absent value, not corruption;
            // anything else that fails to parse is dropped call-site evidence and
            // should be visible rather than silently becoming an empty list.
            if is_meaningful_json_payload(raw) {
                tracing::warn!(%error, "dropping unparseable call_sites payload");
            }
            return Vec::new();
        }
    };
    values
        .into_iter()
        .filter_map(|value| {
            let args = value.get("args")?.as_array()?;
            Some(CallSiteArgs {
                args: args
                    .iter()
                    .filter_map(|arg| arg.as_str().map(str::to_string))
                    .collect(),
            })
        })
        .collect()
}

/// A cell that is empty or a JSON `null` / `[]` is a legitimately-absent value,
/// not corruption — so a parse failure on one of those should not warn.
fn is_meaningful_json_payload(raw: &str) -> bool {
    let trimmed = raw.trim();
    !(trimmed.is_empty() || trimmed == "null" || trimmed == "[]")
}

#[cfg(test)]
mod tests {
    use super::{is_meaningful_json_payload, parse_call_sites};

    #[test]
    fn parse_call_sites_reads_valid_payload() {
        let out = parse_call_sites(r#"[{"args":["a","b"]},{"args":["c"]}]"#);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].args, vec!["a", "b"]);
        assert_eq!(out[1].args, vec!["c"]);
    }

    #[test]
    fn parse_call_sites_returns_empty_on_malformed_or_absent() {
        // Behavior is unchanged (empty on failure) — the refactor only adds a warn.
        assert!(parse_call_sites("not json").is_empty());
        assert!(parse_call_sites("{\"args\":").is_empty());
        assert!(parse_call_sites("").is_empty());
        assert!(parse_call_sites("null").is_empty());
        assert!(parse_call_sites("[]").is_empty());
    }

    #[test]
    fn only_non_absent_payloads_are_worth_warning_about() {
        assert!(!is_meaningful_json_payload(""));
        assert!(!is_meaningful_json_payload("  "));
        assert!(!is_meaningful_json_payload("null"));
        assert!(!is_meaningful_json_payload("[]"));
        assert!(is_meaningful_json_payload("not json"));
        assert!(is_meaningful_json_payload("{\"args\":"));
    }
}
