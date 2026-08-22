//! The `GraphStore` trait implementation for `LadybugStore` — every query is a
//! dialect port of the reference implementation in `cih-falkor/src/query.rs`.
//! Dialect deltas (all spike-verified): `label(r)` not `type(r)`; list
//! indexing is 1-based; bare `ORDER BY` inside `WITH` is rejected, so the
//! shortest-parent trick becomes native `* SHORTEST` recursion (the
//! `RecursiveRel` value carries interior nodes + rel labels — parent and hop
//! kind fall out of it); result caps match the reference exactly.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use cih_core::{Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId, NodeKind};
use cih_graph_store::{
    default_projection_edge_kinds, stored_edge_token, CallSiteArgs, CommunityEdge, CommunityInfo,
    ContextCursorKey, ContextFilter, ContextPage, ContextSection, DbEffect, Direction,
    ExecutionTransition, GraphOverview, GraphOverviewEdge, GraphOverviewNode, GraphProjection,
    GraphProjectionEdge, GraphProjectionNode, GraphProjectionQuery, GraphStore, GraphStoreError,
    GraphSummary, HotspotNode, InterceptingAdvice, Interception, KindCount, LoadObserver,
    LoadStats, NoopObserver, Path, ProjectionNodeRole, ProjectionScope, Result, RouteInfo,
    SimilarMethod, StoredTransition, SymbolContext, TestCoveragePage, TransitionBatch,
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

fn projection_kinds(query: &GraphProjectionQuery) -> Vec<EdgeKind> {
    if query.edge_kinds.is_empty() {
        default_projection_edge_kinds()
    } else {
        query.edge_kinds.clone()
    }
}

fn projection_kind_list(kinds: &[EdgeKind]) -> String {
    kinds
        .iter()
        .map(|kind| cstr(kind.cypher_label()))
        .collect::<Vec<_>>()
        .join(",")
}

fn path_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

async fn projection_rows(store: &LadybugStore, query: String) -> Result<Vec<Vec<Value>>> {
    store
        .with_read_conn(Vec::new(), move |connection| rows(connection, &query))
        .await
}

fn finish_projection(
    mut nodes: Vec<GraphProjectionNode>,
    edge_counts: HashMap<(NodeId, NodeId, EdgeKind), u64>,
    total_nodes: u64,
    node_truncated: bool,
    edge_limit: usize,
) -> GraphProjection {
    nodes.sort_by(|left, right| {
        right
            .member_count
            .cmp(&left.member_count)
            .then_with(|| right.degree.cmp(&left.degree))
            .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });
    nodes.dedup_by(|left, right| left.id == right.id);
    let selected: HashSet<&NodeId> = nodes.iter().map(|node| &node.id).collect();
    let mut edges = edge_counts
        .into_iter()
        .filter(|((source, target, _), _)| {
            selected.contains(source) && selected.contains(target) && source != target
        })
        .map(|((source, target, kind), count)| GraphProjectionEdge {
            source,
            target,
            kind,
            count,
        })
        .collect::<Vec<_>>();
    edges.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.source.as_str().cmp(right.source.as_str()))
            .then_with(|| left.target.as_str().cmp(right.target.as_str()))
            .then_with(|| left.kind.cypher_label().cmp(right.kind.cypher_label()))
    });
    let total_edges = edges.len() as u64;
    let edge_truncated = edges.len() > edge_limit;
    edges.truncate(edge_limit);
    GraphProjection {
        truncated: node_truncated
            || edge_truncated
            || nodes.len() < total_nodes.try_into().unwrap_or(usize::MAX),
        nodes,
        edges,
        total_nodes,
        total_edges,
    }
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

    async fn count_scalar(&self, q: impl Into<String>) -> Result<u64> {
        let q = q.into();
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
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
        // `n.props` rides along so single-node reads expose the persisted
        // props JSON (identity metadata for doc_pack); list queries keep the
        // lean 7-column projection.
        let q = format!(
            "MATCH (n:Symbol {{id: {id}}}) \
             RETURN {columns}, coalesce(n.props, '') LIMIT 1",
            id = cstr(id.as_str())
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        Ok(out.first().map(|r| {
            let mut node = node_from_row(r);
            node.props = r
                .get(7)
                .and_then(|cell| serde_json::from_str(&cell_str(cell)).ok());
            node
        }))
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

    async fn graph_projection(&self, query: &GraphProjectionQuery) -> Result<GraphProjection> {
        query.validate()?;
        let kinds = projection_kinds(query);
        let kind_list = projection_kind_list(&kinds);
        match query.scope {
            ProjectionScope::Repository => {
                let probe = query.node_limit.saturating_add(1);
                let mut rows = projection_rows(
                    self,
                    format!(
                        "MATCH (c:Symbol) WHERE c.kind = 'Community' \
                         RETURN c.id, c.name, coalesce(c.symbolCount, 0) \
                         ORDER BY coalesce(c.symbolCount, 0) DESC, c.id ASC LIMIT {probe}"
                    ),
                )
                .await?;
                let mut node_truncated = rows.len() > query.node_limit;
                rows.truncate(query.node_limit);
                let mut nodes = rows
                    .iter()
                    .filter(|row| row.len() >= 2)
                    .map(|row| GraphProjectionNode {
                        id: NodeId::new(cell_str(&row[0])),
                        kind: NodeKind::Community,
                        name: cell_str(&row[1]),
                        role: ProjectionNodeRole::Aggregate,
                        member_count: row.get(2).map(cell_u64).unwrap_or(0),
                        degree: 0,
                        expandable: true,
                    })
                    .collect::<Vec<_>>();
                let mut total_nodes = self
                    .count_scalar("MATCH (c:Symbol) WHERE c.kind = 'Community' RETURN count(c)")
                    .await?;
                let mut edge_counts = HashMap::new();

                if nodes.is_empty() {
                    let mut folders = projection_rows(
                        self,
                        format!(
                            "MATCH (f:Symbol) WHERE f.kind = 'Folder' AND NOT f.file CONTAINS '/' \
                             RETURN f.id, f.name ORDER BY f.id ASC LIMIT {probe}"
                        ),
                    )
                    .await?;
                    node_truncated |= folders.len() > query.node_limit;
                    folders.truncate(query.node_limit);
                    total_nodes = self
                        .count_scalar(
                            "MATCH (f:Symbol) WHERE f.kind = 'Folder' AND NOT f.file CONTAINS '/' RETURN count(f)",
                        )
                        .await?;
                    nodes = folders
                        .into_iter()
                        .filter(|row| row.len() >= 2)
                        .map(|row| GraphProjectionNode {
                            id: NodeId::new(cell_str(&row[0])),
                            kind: NodeKind::Folder,
                            name: cell_str(&row[1]),
                            role: ProjectionNodeRole::Aggregate,
                            member_count: 0,
                            degree: 0,
                            expandable: false,
                        })
                        .collect();
                } else {
                    let edge_probe = query.edge_limit.saturating_add(1);
                    let ids = nodes
                        .iter()
                        .map(|node| cstr(node.id.as_str()))
                        .collect::<Vec<_>>()
                        .join(",");
                    let edge_rows = projection_rows(
                        self,
                        format!(
                            "MATCH (a:Symbol)-[:MEMBER_OF]->(ca:Symbol), \
                                   (b:Symbol)-[:MEMBER_OF]->(cb:Symbol), \
                                   (a)-[r]->(b) \
                             WHERE ca.id IN [{ids}] AND cb.id IN [{ids}] AND ca.id <> cb.id \
                               AND label(r) IN [{kind_list}] \
                             RETURN ca.id, cb.id, label(r), count(*) \
                             ORDER BY count(*) DESC, ca.id ASC, cb.id ASC LIMIT {edge_probe}"
                        ),
                    )
                    .await?;
                    for row in edge_rows.iter().filter(|row| row.len() >= 4) {
                        *edge_counts
                            .entry((
                                NodeId::new(cell_str(&row[0])),
                                NodeId::new(cell_str(&row[1])),
                                edge_from_label(&cell_str(&row[2])),
                            ))
                            .or_insert(0) += cell_u64(&row[3]);
                    }
                    let mut degree = HashMap::<NodeId, u64>::new();
                    for ((source, target, _), count) in &edge_counts {
                        *degree.entry(source.clone()).or_insert(0) += *count;
                        *degree.entry(target.clone()).or_insert(0) += *count;
                    }
                    for node in &mut nodes {
                        node.degree = degree.get(&node.id).copied().unwrap_or(0);
                    }
                }
                Ok(finish_projection(
                    nodes,
                    edge_counts,
                    total_nodes,
                    node_truncated,
                    query.edge_limit,
                ))
            }
            ProjectionScope::Community => {
                let parent = query
                    .parent_id
                    .as_ref()
                    .expect("validated community parent");
                let parent_node = self
                    .get_node(parent)
                    .await?
                    .ok_or_else(|| GraphStoreError::NotFound(parent.to_string()))?;
                if parent_node.kind != NodeKind::Community {
                    return Err(GraphStoreError::InvalidInput(
                        "community projection parent must be a Community node".into(),
                    ));
                }
                let parent_lit = cstr(parent.as_str());
                let probe = query.node_limit.saturating_add(1);
                let mut file_rows = projection_rows(
                    self,
                    format!(
                        "MATCH (n:Symbol)-[:MEMBER_OF]->(c:Symbol) \
                         WHERE c.id = {parent_lit} AND n.file <> '' \
                         RETURN n.file, count(n) ORDER BY count(n) DESC, n.file ASC LIMIT {probe}"
                    ),
                )
                .await?;
                let mut node_truncated = file_rows.len() > query.node_limit;
                file_rows.truncate(query.node_limit);
                let total_files = self
                    .count_scalar(format!(
                        "MATCH (n:Symbol)-[:MEMBER_OF]->(c:Symbol) \
                         WHERE c.id = {parent_lit} AND n.file <> '' \
                         WITH DISTINCT n.file AS file RETURN count(file)"
                    ))
                    .await?;
                let mut nodes = file_rows
                    .iter()
                    .filter(|row| row.len() >= 2)
                    .map(|row| {
                        let path = cell_str(&row[0]);
                        let members = cell_u64(&row[1]);
                        GraphProjectionNode {
                            id: NodeId::new(format!("File:{path}")),
                            kind: NodeKind::File,
                            name: path_name(&path),
                            role: ProjectionNodeRole::Aggregate,
                            member_count: members,
                            degree: members,
                            expandable: true,
                        }
                    })
                    .collect::<Vec<_>>();
                let selected_files = file_rows
                    .iter()
                    .filter_map(|row| row.first().map(cell_str))
                    .collect::<Vec<_>>();
                let file_list = selected_files
                    .iter()
                    .map(|file| cstr(file))
                    .collect::<Vec<_>>()
                    .join(",");
                let mut edge_counts = HashMap::new();
                if !selected_files.is_empty() {
                    let edge_probe = query.edge_limit.saturating_add(1);
                    let internal = projection_rows(
                        self,
                        format!(
                            "MATCH (a:Symbol)-[:MEMBER_OF]->(c:Symbol)<-[:MEMBER_OF]-(b:Symbol), \
                                   (a)-[r]->(b) \
                             WHERE c.id = {parent_lit} AND a.file IN [{file_list}] \
                               AND b.file IN [{file_list}] AND a.file <> b.file \
                               AND label(r) IN [{kind_list}] \
                             RETURN a.file, b.file, label(r), count(*) \
                             ORDER BY count(*) DESC, a.file ASC, b.file ASC LIMIT {edge_probe}"
                        ),
                    )
                    .await?;
                    for row in internal.iter().filter(|row| row.len() >= 4) {
                        *edge_counts
                            .entry((
                                NodeId::new(format!("File:{}", cell_str(&row[0]))),
                                NodeId::new(format!("File:{}", cell_str(&row[1]))),
                                edge_from_label(&cell_str(&row[2])),
                            ))
                            .or_insert(0) += cell_u64(&row[3]);
                    }

                    let boundary_budget = query
                        .boundary_limit
                        .min(query.node_limit.saturating_sub(nodes.len()));
                    let boundary_probe = boundary_budget.saturating_add(1);
                    let outgoing = projection_rows(
                        self,
                        format!(
                            "MATCH (a:Symbol)-[:MEMBER_OF]->(c:Symbol), \
                                   (b:Symbol)-[:MEMBER_OF]->(other:Symbol), (a)-[r]->(b) \
                             WHERE c.id = {parent_lit} AND other.id <> c.id \
                               AND a.file IN [{file_list}] AND label(r) IN [{kind_list}] \
                             RETURN a.file, other.id, other.name, label(r), count(*) \
                             ORDER BY count(*) DESC, a.file ASC, other.id ASC LIMIT {boundary_probe}"
                        ),
                    )
                    .await?;
                    node_truncated |= outgoing.len() > boundary_budget;
                    for row in outgoing.iter().take(boundary_budget) {
                        if row.len() < 5 {
                            continue;
                        }
                        let boundary = NodeId::new(cell_str(&row[1]));
                        if !nodes.iter().any(|node| node.id == boundary)
                            && nodes
                                .iter()
                                .filter(|node| node.role == ProjectionNodeRole::Boundary)
                                .count()
                                < boundary_budget
                        {
                            nodes.push(GraphProjectionNode {
                                id: boundary.clone(),
                                kind: NodeKind::Community,
                                name: cell_str(&row[2]),
                                role: ProjectionNodeRole::Boundary,
                                member_count: 0,
                                degree: cell_u64(&row[4]),
                                expandable: true,
                            });
                        }
                        *edge_counts
                            .entry((
                                NodeId::new(format!("File:{}", cell_str(&row[0]))),
                                boundary,
                                edge_from_label(&cell_str(&row[3])),
                            ))
                            .or_insert(0) += cell_u64(&row[4]);
                    }
                }
                let total_nodes = total_files.saturating_add(
                    nodes
                        .iter()
                        .filter(|node| node.role == ProjectionNodeRole::Boundary)
                        .count() as u64,
                );
                Ok(finish_projection(
                    nodes,
                    edge_counts,
                    total_nodes,
                    node_truncated,
                    query.edge_limit,
                ))
            }
            ProjectionScope::File => {
                let parent = query.parent_id.as_ref().expect("validated file parent");
                let parent_node = self
                    .get_node(parent)
                    .await?
                    .ok_or_else(|| GraphStoreError::NotFound(parent.to_string()))?;
                if parent_node.kind != NodeKind::File {
                    return Err(GraphStoreError::InvalidInput(
                        "file projection parent must be a File node".into(),
                    ));
                }
                let file = parent_node.file;
                let file_lit = cstr(&file);
                let probe = query.node_limit.saturating_add(1);
                let columns = node_columns("n");
                let mut symbol_rows = projection_rows(
                    self,
                    format!(
                        "MATCH (n:Symbol) WHERE n.file = {file_lit} \
                           AND NOT n.kind IN ['File','Folder','Community','Process'] \
                         OPTIONAL MATCH (n)-[r]-(:Symbol) WITH n, count(r) AS degree \
                         RETURN {columns}, degree ORDER BY degree DESC, n.id ASC LIMIT {probe}"
                    ),
                )
                .await?;
                let node_truncated = symbol_rows.len() > query.node_limit;
                symbol_rows.truncate(query.node_limit);
                let total_symbols = self
                    .count_scalar(format!(
                        "MATCH (n:Symbol) WHERE n.file = {file_lit} \
                         AND NOT n.kind IN ['File','Folder','Community','Process'] RETURN count(n)"
                    ))
                    .await?;
                let nodes = symbol_rows
                    .iter()
                    .filter(|row| row.len() >= 8)
                    .map(|row| {
                        let node = node_from_row(&row[..7]);
                        GraphProjectionNode {
                            id: node.id,
                            kind: node.kind,
                            name: node.name,
                            role: ProjectionNodeRole::Entity,
                            member_count: 1,
                            degree: cell_u64(&row[7]),
                            expandable: true,
                        }
                    })
                    .collect::<Vec<_>>();
                let ids = nodes
                    .iter()
                    .map(|node| cstr(node.id.as_str()))
                    .collect::<Vec<_>>()
                    .join(",");
                let mut edge_counts = HashMap::new();
                if !nodes.is_empty() {
                    let edge_probe = query.edge_limit.saturating_add(1);
                    let internal = projection_rows(
                        self,
                        format!(
                            "MATCH (a:Symbol)-[r]->(b:Symbol) WHERE a.id IN [{ids}] \
                               AND b.id IN [{ids}] AND label(r) IN [{kind_list}] \
                             RETURN a.id, b.id, label(r), count(*) \
                             ORDER BY count(*) DESC, a.id ASC, b.id ASC LIMIT {edge_probe}"
                        ),
                    )
                    .await?;
                    for row in internal.iter().filter(|row| row.len() >= 4) {
                        *edge_counts
                            .entry((
                                NodeId::new(cell_str(&row[0])),
                                NodeId::new(cell_str(&row[1])),
                                edge_from_label(&cell_str(&row[2])),
                            ))
                            .or_insert(0) += cell_u64(&row[3]);
                    }
                }
                Ok(finish_projection(
                    nodes,
                    edge_counts,
                    total_symbols,
                    node_truncated,
                    query.edge_limit,
                ))
            }
        }
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

    async fn nodes_in_files(&self, files: &[String], limit: usize) -> Result<Vec<Node>> {
        if files.is_empty() {
            return Ok(vec![]);
        }
        let list = format!(
            "[{}]",
            files.iter().map(|f| cstr(f)).collect::<Vec<_>>().join(", ")
        );
        // Bounded `LIMIT` with a deterministic order: keeps a single huge generated
        // file from loading an unbounded symbol set, and makes truncation stable.
        let columns = node_columns("n");
        let q = format!(
            "MATCH (n:Symbol) WHERE n.file IN {list} \
               AND n.kind IN ['Method', 'Constructor', 'Function', 'Class', 'Interface', 'Enum'] \
             RETURN {columns} ORDER BY n.file, n.id LIMIT {limit}"
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

    async fn test_coverage_page(&self, id: &NodeId, limit: usize) -> Result<TestCoveragePage> {
        if limit == 0 {
            return Err(GraphStoreError::InvalidInput(
                "test_coverage_page limit must be at least 1".into(),
            ));
        }
        // The queried node's kind selects the scope; a missing node is an
        // empty complete page, not an error (callers gate identity separately).
        let Some(node) = self.get_node(id).await? else {
            return Ok(TestCoveragePage {
                tests: Vec::new(),
                has_more: false,
            });
        };
        let id_lit = cstr(id.as_str());
        let columns = node_columns("t");
        let probe = limit.saturating_add(1);
        // Kùzu dialect: `EXISTS { MATCH … }` subqueries (the pattern-predicate
        // form FalkorDB uses is not accepted here — mirrors the legacy query).
        let scope = match node.kind {
            NodeKind::Class | NodeKind::Interface => format!(
                "target.id = {id_lit} \
                 OR EXISTS {{ \
                       MATCH (owner:Symbol)-[:HAS_METHOD]->(member:Symbol) \
                       WHERE owner.id = {id_lit} AND member.id = target.id \
                    }}"
            ),
            NodeKind::Method | NodeKind::Constructor => format!(
                "target.id = {id_lit} \
                 OR EXISTS {{ \
                       MATCH (owner:Symbol)-[:HAS_METHOD]->(member:Symbol) \
                       WHERE member.id = {id_lit} AND owner.id = target.id \
                    }}"
            ),
            _ => format!("target.id = {id_lit}"),
        };
        let q = format!(
            "MATCH (t:Symbol)-[:TESTS]->(target:Symbol) \
             WHERE {scope} \
             RETURN DISTINCT {columns} \
             ORDER BY t.file, t.name, t.id LIMIT {probe}"
        );
        let out = self
            .with_read_conn(Vec::new(), move |conn| rows(conn, &q))
            .await?;
        let mut tests: Vec<Node> = out.iter().map(|r| node_from_row(r)).collect();
        let has_more = tests.len() > limit;
        tests.truncate(limit);
        Ok(TestCoveragePage { tests, has_more })
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

fn files_is_empty(file_list_literal: &str) -> bool {
    file_list_literal == "[]"
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
