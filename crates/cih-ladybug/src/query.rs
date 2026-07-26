//! The `GraphStore` trait implementation for `LadybugStore` — every query is a
//! dialect port of the reference implementation in `cih-falkor/src/query.rs`.
//! Dialect deltas (all spike-verified): `label(r)` not `type(r)`; list
//! indexing is 1-based; bare `ORDER BY` inside `WITH` is rejected, so the
//! shortest-parent trick becomes native `* SHORTEST` recursion (the
//! `RecursiveRel` value carries interior nodes + rel labels — parent and hop
//! kind fall out of it); result caps match the reference exactly.

use std::cmp::Ordering;
use std::collections::HashSet;

use async_trait::async_trait;
use cih_core::{Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId};
use cih_graph_store::{
    stored_edge_token, CallSiteArgs, CommunityEdge, CommunityInfo, ContextCursorKey, ContextFilter,
    ContextPage, ContextSection, DbEffect, Direction, ExecutionTransition, GraphOverview,
    GraphOverviewEdge, GraphOverviewNode, GraphStore, GraphStoreError, GraphSummary, HotspotNode,
    InterceptingAdvice, Interception, KindCount, LoadObserver, LoadStats, NoopObserver, Path,
    Result, RouteInfo, SimilarMethod, StoredTransition, SymbolContext, TransitionBatch,
    TransitionQuery, EXECUTION_BATCH_SIZE,
};
use lbug::{Connection, Value};

use crate::convert::{cell_f64, cell_str, cell_u64, cstr, node_from_row, recursive_rel};
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

/// Canonical column order consumed by `node_from_row`. Keep every query that
/// returns a domain `Node` on this projection so source ranges cannot silently
/// disappear when a new read path is added.
fn node_columns(alias: &str) -> String {
    format!(
        "{alias}.id, {alias}.kind, {alias}.name, {alias}.qn, \
         {alias}.file, {alias}.sl, {alias}.el"
    )
}

impl LadybugStore {
    /// CALLS neighbors as full nodes (context callers/callees).
    async fn neighbor_nodes(
        &self,
        id: &NodeId,
        dir: Direction,
        limit: usize,
        after: Option<&ContextCursorKey>,
    ) -> Result<ContextSection<Node>> {
        let arrow = match dir {
            Direction::Upstream => "<-[:CALLS]-",
            Direction::Downstream => "-[:CALLS]->",
            Direction::Both => "-[:CALLS]-",
        };
        let columns = node_columns("m");
        let cursor_predicate = after.map_or_else(String::new, |after| {
            format!(
                "WHERE m.name > {} OR (m.name = {} AND m.id > {}) ",
                cstr(&after.name),
                cstr(&after.name),
                cstr(&after.id)
            )
        });
        let probe_limit = limit + 1;
        let q = format!(
            "MATCH (n:Symbol {{id: {id}}}){arrow}(m:Symbol) \
             {cursor_predicate}\
             RETURN DISTINCT {columns} \
             ORDER BY m.name, m.id LIMIT {probe_limit}",
            id = cstr(id.as_str())
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        let nodes = out.iter().map(|r| node_from_row(r)).collect();
        Ok(ContextSection::from_node_probe(nodes, limit, after))
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
        let columns = node_columns("n");
        let q = format!(
            "MATCH (n:Symbol {{id: {id}}}) \
             RETURN {columns} LIMIT 1",
            id = cstr(id.as_str())
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out.first().map(|r| node_from_row(r)))
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
            let cursor = transition_cursor_predicate(query.after.as_ref(), false, "label(r)")
                .map_or_else(String::new, |predicate| format!("AND ({predicate}) "));
            let q = format!(
                "MATCH (s:Symbol)-[r{rel}]->(t:Symbol) \
                 WHERE s.id IN [{list}] {cursor}\
                 RETURN s.id, {columns}, label(r), coalesce(r.confidence, 1.0), \
                        coalesce(r.reason, ''), coalesce(r.callSites, '') \
                 ORDER BY s.id, coalesce(t.name, ''), t.id, label(r) LIMIT {probe_limit}"
            );
            let out = self
                .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
                .await?;
            transitions.extend(parse_stored_rows(out, false));
        }
        if matches!(query.direction, Direction::Upstream | Direction::Both) {
            let cursor = transition_cursor_predicate(query.after.as_ref(), true, "label(r)")
                .map_or_else(String::new, |predicate| format!("AND ({predicate}) "));
            let q = format!(
                "MATCH (s:Symbol)<-[r{rel}]-(t:Symbol) \
                 WHERE s.id IN [{list}] {cursor}\
                 RETURN s.id, {columns}, label(r), coalesce(r.confidence, 1.0), \
                        coalesce(r.reason, ''), coalesce(r.callSites, '') \
                 ORDER BY s.id, coalesce(t.name, ''), t.id, label(r) LIMIT {probe_limit}"
            );
            let out = self
                .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
                .await?;
            transitions.extend(parse_stored_rows(out, true));
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
            "CALLS|:EXTERNAL_CALL|:PUBLISHES_EVENT|:EXECUTES_QUERY|:READS_TABLE|:WRITES_TABLE"
        } else {
            "CALLS|:EXTERNAL_CALL|:PUBLISHES_EVENT"
        };
        let limit = limit.clamp(1, 50_001);
        let target_columns = node_columns("t");
        let forward = format!(
            "MATCH (s:Symbol)-[r:{outgoing}]->(t:Symbol) \
             WHERE s.id IN [{list}] \
             RETURN s.id, {target_columns}, coalesce(t.isAccessor, 0), label(r), \
                    coalesce(r.confidence, 1.0), coalesce(r.reason, ''), \
                    coalesce(r.callSites, '') \
             ORDER BY t.name, t.id, s.id, label(r) LIMIT {limit}"
        );
        let reverse = format!(
            "MATCH (s:Symbol)<-[r:HANDLES_ROUTE|:LISTENS_TO]-(t:Symbol) \
             WHERE s.id IN [{list}] \
             RETURN s.id, {target_columns}, coalesce(t.isAccessor, 0), label(r), \
                    coalesce(r.confidence, 1.0), coalesce(r.reason, ''), \
                    coalesce(r.callSites, '') \
             ORDER BY t.name, t.id, s.id, label(r) LIMIT {limit}"
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &forward))
            .await?;
        let mut transitions = parse_execution_rows(out, false);
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &reverse))
            .await?;
        transitions.extend(parse_execution_rows(out, true));
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
                "MATCH (a:Symbol)-[r:ADVISES]->(m:Symbol) WHERE m.id IN [{list}] \
                 RETURN m.id, a.id, r.reason"
            );
            let out = self
                .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
                .await?;
            matches.extend(out.into_iter().filter_map(|row| {
                if row.len() < 3 {
                    return None;
                }
                let reason = cell_str(&row[2]);
                let advice_kind = reason.strip_prefix("aop-").unwrap_or(&reason).to_string();
                Some(Interception {
                    target: NodeId::new(cell_str(&row[0])),
                    advice: InterceptingAdvice {
                        advice: NodeId::new(cell_str(&row[1])),
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
            let columns = node_columns("n");
            let q = format!(
                "MATCH (n:Symbol) WHERE n.kind IN [{kind_literals}] \
                 OPTIONAL MATCH (n)-[r]-(:Symbol) \
                 WITH n, count(r) AS degree ORDER BY degree DESC, n.id ASC LIMIT {max_nodes} \
                 RETURN {columns}, degree"
            );
            let out = self
                .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
                .await?;
            for r in out.iter().filter(|r| r.len() >= 8) {
                push_row(&mut nodes, &mut selected, r, cell_u64(&r[7]));
            }
        } else {
            let structural = "['Community','Process','Route','IntegrationRoute',\
                 'MessageDestination','KafkaTopic','ExternalEndpoint','DbTable','DbQuery']";
            let pass1_limit = max_nodes.min(2_000);
            let columns = node_columns("n");
            let q1 = format!(
                "MATCH (n:Symbol) WHERE n.kind IN {structural} \
                 RETURN {columns} LIMIT {pass1_limit}"
            );
            let out = self
                .with_read_conn(Vec::new(), move |conn| rows(conn, &q1))
                .await?;
            for r in out.iter().filter(|r| r.len() >= 7) {
                push_row(&mut nodes, &mut selected, r, 0);
            }
            let remaining = max_nodes.saturating_sub(nodes.len());
            if remaining > 0 {
                let columns = node_columns("n");
                let q2 = format!(
                    "MATCH (n:Symbol) WHERE n.kind IN ['Class','Interface','Enum','Record'] \
                     OPTIONAL MATCH (n)-[r]-(:Symbol) \
                     WITH n, count(r) AS degree ORDER BY degree DESC, n.id ASC LIMIT {remaining} \
                     RETURN {columns}, degree"
                );
                let out = self
                    .with_read_conn(Vec::new(), move |conn| rows(conn, &q2))
                    .await?;
                for r in out.iter().filter(|r| r.len() >= 8) {
                    push_row(&mut nodes, &mut selected, r, cell_u64(&r[7]));
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
        let process_predicate = filter
            .process_after
            .as_ref()
            .map_or_else(String::new, |after| {
                format!(
                    "AND (p.name > {} OR (p.name = {} AND p.id > {})) ",
                    cstr(&after.name),
                    cstr(&after.name),
                    cstr(&after.id)
                )
            });
        let process_probe = filter.process_limit + 1;
        let proc_q = format!(
            "MATCH (s:Symbol {{id: {id}}})-[:STEP_IN_PROCESS]->(p:Symbol) \
             WHERE p.kind = 'Process' {process_predicate}\
             RETURN DISTINCT p.name, p.id \
             ORDER BY p.name, p.id LIMIT {process_probe}",
            id = cstr(id.as_str())
        );
        let process_rows = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &proc_q))
            .await?;
        let processes = ContextSection::from_process_probe(
            process_rows
                .into_iter()
                .filter(|row| row.len() >= 2)
                .map(|row| (cell_str(&row[0]), cell_str(&row[1])))
                .collect(),
            filter.process_limit,
            filter.process_after.as_ref(),
        );
        let callers = self
            .neighbor_nodes(
                id,
                Direction::Upstream,
                filter.caller_limit,
                filter.caller_after.as_ref(),
            )
            .await?;
        let callees = self
            .neighbor_nodes(
                id,
                Direction::Downstream,
                filter.callee_limit,
                filter.callee_after.as_ref(),
            )
            .await?;
        let community = self
            .symbol_communities(std::slice::from_ref(id))
            .await?
            .into_iter()
            .find_map(|(nid, info)| if &nid == id { Some(info) } else { None });
        Ok(ContextPage {
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
        let columns = node_columns("n");
        let q = format!(
            "MATCH (n:Symbol) WHERE n.name = {name} \
             RETURN {columns} ORDER BY n.id LIMIT {lim}",
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
        let columns = node_columns("n");
        let q = format!(
            "MATCH (n:Symbol) WHERE n.file IN {list} \
               AND n.kind IN ['Method', 'Constructor', 'Function', 'Class', 'Interface', 'Enum'] \
             RETURN {columns} ORDER BY n.file, n.id"
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
        // JSON (not promoted columns) — parse client-side.
        let q = format!(
            "MATCH (m:Symbol)-[:EXECUTES_QUERY]->(q:Symbol)-[r:READS_TABLE|:WRITES_TABLE]->(t:Symbol) \
             WHERE m.id IN {list} \
             RETURN m.id, q.id, coalesce(q.props, ''), t.name, label(r) \
             ORDER BY m.id, t.name"
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out
            .into_iter()
            .filter(|r| r.len() >= 5)
            .map(|r| {
                DbEffect::from_query_row(
                    NodeId::new(cell_str(&r[0])),
                    NodeId::new(cell_str(&r[1])),
                    &cell_str(&r[2]),
                    cell_str(&r[3]),
                    &cell_str(&r[4]),
                )
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
        let columns = node_columns("t");
        let q = format!(
            "MATCH (t:Symbol)-[:TESTS]->(target:Symbol) \
             WHERE target.id = {id_lit} \
                OR EXISTS {{ \
                      MATCH (owner:Symbol)-[:HAS_METHOD]->(target2:Symbol) \
                      WHERE target2.id = {id_lit} AND owner.id = target.id \
                   }} \
             RETURN DISTINCT {columns} \
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
        let columns = node_columns("t");
        let q1 = format!(
            "MATCH (t:Symbol)-[:TESTS]->(prod:Symbol) WHERE prod.file IN {list} \
             RETURN DISTINCT {columns} \
             ORDER BY t.file, t.name LIMIT 200"
        );
        let q2 = format!(
            "MATCH (t:Symbol)-[:TESTS]->(:Symbol)-[:CALLS]->(prod:Symbol) \
             WHERE prod.file IN {list} \
             RETURN DISTINCT {columns} \
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
        let columns = node_columns("n");
        // `stereotype IS NULL OR <> 'test'` spells out the intended semantics
        // (a missing stereotype is not a test) rather than relying on
        // three-valued NOT like the reference query.
        let q = format!(
            "MATCH (n:Symbol) \
             WHERE n.file STARTS WITH {prefix_lit} \
               AND n.kind IN ['Method', 'Class', 'Interface'] \
               AND (n.stereotype IS NULL OR n.stereotype <> 'test') \
               AND NOT EXISTS {{ MATCH (:Symbol)-[:TESTS]->(n) }} \
             RETURN {columns} \
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

fn parse_execution_rows(
    rows: Vec<Vec<Value>>,
    traversed_reverse: bool,
) -> Vec<ExecutionTransition> {
    rows.into_iter()
        .filter_map(|row| {
            if row.len() < 13 {
                return None;
            }
            Some(ExecutionTransition {
                source: NodeId::new(cell_str(&row[0])),
                target: node_from_row(&row[1..8]),
                target_is_accessor: cell_u64(&row[8]) == 1,
                kind: cell_str(&row[9]),
                confidence: cell_f64(&row[10]) as f32,
                reason: cell_str(&row[11]),
                call_sites: parse_call_sites(&cell_str(&row[12])),
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

fn parse_stored_rows(rows: Vec<Vec<Value>>, traversed_reverse: bool) -> Vec<StoredTransition> {
    rows.into_iter()
        .filter_map(|row| {
            if row.len() < 12 {
                return None;
            }
            let source = NodeId::new(cell_str(&row[0]));
            let target = node_from_row(&row[1..8]);
            let kind = edge_from_label(&cell_str(&row[8]));
            let (stored_src, stored_dst) = if traversed_reverse {
                (target.id.clone(), source.clone())
            } else {
                (source.clone(), target.id.clone())
            };
            let call_sites = serde_json::from_str::<serde_json::Value>(&cell_str(&row[11]))
                .ok()
                .filter(|value| value.as_array().is_some_and(|items| !items.is_empty()))
                .map(|value| serde_json::json!({"call_sites": value}));
            let edge = Edge {
                src: stored_src.clone(),
                dst: stored_dst.clone(),
                kind,
                confidence: cell_f64(&row[9]) as f32,
                reason: cell_str(&row[10]),
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
    serde_json::from_str::<Vec<serde_json::Value>>(raw)
        .unwrap_or_default()
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

fn files_is_empty(file_list_literal: &str) -> bool {
    file_list_literal == "[]"
}
