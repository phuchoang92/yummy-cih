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
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use cih_core::{Edge, EdgeKind, GraphArtifacts, Node, NodeId, NodeKind};
use cih_graph_store::{
    BulkLoader, Direction, FlowNode, GraphStore, GraphStoreError, LoadStats, Result,
};
use redis::Value;

use serialize::*;

mod bulk;
mod query;
mod serialize;

/// Rows per UNWIND batch during bulk load. Larger batches cut Redis round-trips on big graphs
/// (~2M edges at 600k nodes) at the cost of bigger per-statement strings — 4000 is a good balance.
const BATCH: usize = 4000;

/// Default max wait for a query permit before shedding (used when no explicit
/// limit is configured, e.g. the engine bulk-load path).
const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct FalkorStore {
    client: redis::Client,
    graph_key: String,
    /// Lazily-built, auto-reconnecting, cloneable multiplexed connection. Built
    /// once on first query and shared across all calls — replaces opening a
    /// fresh connection per query.
    conn: tokio::sync::OnceCell<redis::aio::ConnectionManager>,
    /// Bounds concurrent `GRAPH.QUERY` execution (backpressure). Defaults to
    /// effectively unlimited; the server tightens it via [`Self::with_query_limit`].
    query_limit: Arc<tokio::sync::Semaphore>,
    /// Max time to wait for a permit before shedding with an "overloaded" error.
    acquire_timeout: Duration,
}

impl FalkorStore {
    pub fn connect(url: &str, graph_key: impl Into<String>) -> Result<Self> {
        let client =
            redis::Client::open(url).map_err(|e| GraphStoreError::Backend(e.to_string()))?;
        Ok(Self {
            client,
            graph_key: graph_key.into(),
            conn: tokio::sync::OnceCell::new(),
            // Effectively unlimited by default — the engine's sequential bulk-load
            // path must never be throttled. The server opts into a real bound.
            query_limit: Arc::new(tokio::sync::Semaphore::new(
                tokio::sync::Semaphore::MAX_PERMITS,
            )),
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
        })
    }

    /// Bound concurrent Cypher queries to `max_concurrent`, shedding requests
    /// that can't acquire a permit within `acquire_timeout`. Used by the MCP
    /// server to apply backpressure under multi-client load.
    pub fn with_query_limit(mut self, max_concurrent: usize, acquire_timeout: Duration) -> Self {
        self.query_limit = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
        self.acquire_timeout = acquire_timeout;
        self
    }

    /// A clone of the shared, reconnecting connection, building it on first use.
    async fn conn(&self) -> Result<redis::aio::ConnectionManager> {
        self.conn
            .get_or_try_init(|| redis::aio::ConnectionManager::new(self.client.clone()))
            .await
            .cloned()
            .map_err(|e| GraphStoreError::Backend(e.to_string()))
    }

    /// Acquire a query permit, shedding with an "overloaded" error if the
    /// concurrency limit is saturated for longer than `acquire_timeout`.
    async fn acquire_permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit> {
        match tokio::time::timeout(
            self.acquire_timeout,
            self.query_limit.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => Ok(permit),
            // Semaphore was closed — the store has been dropped; retrying is futile.
            Ok(Err(_)) => Err(GraphStoreError::Backend(
                "graph store shut down: query semaphore closed".into(),
            )),
            // Timeout elapsed — transient overload, caller may retry.
            Err(_) => Err(GraphStoreError::Backend(
                "graph store overloaded: concurrent query limit reached".into(),
            )),
        }
    }

    async fn run(&self, cypher: &str) -> Result<Value> {
        let _permit = self.acquire_permit().await?;
        let mut con = self.conn().await?;
        redis::cmd("GRAPH.QUERY")
            .arg(&self.graph_key)
            .arg(cypher)
            .query_async(&mut con)
            .await
            .map_err(|e| GraphStoreError::Backend(e.to_string()))
    }

    async fn graph_command(&self, command: &str, args: &[&str]) -> Result<Value> {
        let _permit = self.acquire_permit().await?;
        let mut con = self.conn().await?;
        let mut cmd = redis::cmd(command);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.query_async(&mut con)
            .await
            .map_err(|e| GraphStoreError::Backend(e.to_string()))
    }

    pub async fn drop_graph(&self) -> Result<()> {
        match self.graph_command("GRAPH.DELETE", &[&self.graph_key]).await {
            Ok(_) => Ok(()),
            // GRAPH.DELETE on a nonexistent key errors "Invalid graph operation on
            // empty key". Dropping an absent graph is a no-op success — this makes
            // `drop_graph` idempotent (e.g. after `publish_to` RENAMEs staging away).
            Err(GraphStoreError::Backend(msg)) if msg.contains("empty key") => Ok(()),
            Err(e) => Err(e),
        }
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

    /// True when a backend error is FalkorDB/Redis reporting it is still loading
    /// its persisted dataset into memory (`BusyLoadingError`). Transient at
    /// startup — especially when one instance holds a large (multi-GB) AOF/RDB.
    fn is_loading_error(e: &GraphStoreError) -> bool {
        matches!(e, GraphStoreError::Backend(msg) if msg.to_ascii_lowercase().contains("loading"))
    }

    /// Max time to wait for a loading FalkorDB before giving up, from
    /// `CIH_FALKOR_LOAD_WAIT_SECS` (default 600s — enough for a multi-GB reload).
    fn load_wait_budget() -> Duration {
        Duration::from_secs(
            std::env::var("CIH_FALKOR_LOAD_WAIT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(600),
        )
    }

    /// Block until FalkorDB can serve a trivial query, tolerating the
    /// `BusyLoadingError` window while it loads a large dataset. Polls with
    /// exponential backoff (capped at 5s) up to `max_wait`, logging progress so a
    /// multi-minute wait is visible. Any non-loading error fails fast.
    async fn wait_until_ready(&self, max_wait: Duration) -> Result<()> {
        let start = std::time::Instant::now();
        let mut delay = Duration::from_millis(200);
        let mut next_log = Duration::from_secs(5);
        loop {
            match self.run("RETURN 1").await {
                Ok(_) => return Ok(()),
                Err(e) if Self::is_loading_error(&e) => {
                    if start.elapsed() >= max_wait {
                        return Err(GraphStoreError::Backend(format!(
                            "FalkorDB still loading its dataset after {}s — artifacts are on \
                             disk; re-run once it is ready, or raise CIH_FALKOR_LOAD_WAIT_SECS",
                            max_wait.as_secs()
                        )));
                    }
                    if start.elapsed() >= next_log {
                        tracing::info!(
                            elapsed_s = start.elapsed().as_secs(),
                            "waiting for FalkorDB to finish loading its dataset…"
                        );
                        next_log += Duration::from_secs(15);
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Run a write query, waiting out a `BusyLoadingError` (e.g. FalkorDB
    /// restarted mid-load) once before retrying — so a batch never fails just
    /// because the instance was momentarily loading.
    async fn run_write(&self, cypher: &str) -> Result<Value> {
        match self.run(cypher).await {
            Err(e) if Self::is_loading_error(&e) => {
                self.wait_until_ready(Self::load_wait_budget()).await?;
                self.run(cypher).await
            }
            other => other,
        }
    }

    /// Core write path: MERGE nodes then edges in UNWIND batches. Idempotent
    /// (re-running the same artifact is a no-op), so it doubles as upsert.
    async fn load_nodes_edges(&self, nodes: &[Node], edges: &[Edge]) -> Result<LoadStats> {
        let _ = self.run("CREATE INDEX FOR (n:Symbol) ON (n.id)").await; // idempotent

        let node_chunks: Vec<_> = nodes.chunks(BATCH).collect();
        let total_node_batches = node_chunks.len();
        for (batch_idx, chunk) in node_chunks.into_iter().enumerate() {
            // ON CREATE SET only: the staging graph is always drop_graph()'d before
            // bulk_load runs, and upsert_incremental DETACH-DELETEs changed-file nodes
            // before calling load_nodes_edges. So every MERGE here creates a new node.
            // In the retry case (partial batch failure), matched nodes already hold the
            // correct values from the same artifact — skipping ON MATCH SET is safe and
            // avoids re-writing ~18 properties per node for zero net change.
            let q = format!(
                "UNWIND {arr} AS row \
                 MERGE (n:Symbol {{id: row.id}}) \
                 ON CREATE SET n.name = row.name, n.kind = row.kind, n.file = row.file, \
                     n.qualifiedName = row.qn, n.startLine = row.sl, n.endLine = row.el, \
                     n.props = row.props, n.stereotype = row.stereotype, \
                     n.httpMethod = row.httpMethod, n.path = row.path, \
                     n.decorator = row.decorator, n.handler = row.handler, \
                     n.symbolCount = row.symbolCount, n.cohesion = row.cohesion, \
                     n.processType = row.processType, \
                     n.cyclomatic = row.cyclomatic, n.cognitive = row.cognitive, \
                     n.loopDepth = row.loopDepth, n.transitiveLoopDepth = row.transitiveLoopDepth",
                arr = nodes_to_list(chunk)
            );
            self.run_write(&q).await.inspect_err(|_| {
                tracing::error!(
                    batch = batch_idx,
                    committed_batches = batch_idx,
                    total_batches = total_node_batches,
                    "node batch write failed — graph is partially written; \
                     re-run bulk_load from scratch to restore consistency"
                );
            })?;
        }

        // Relationship types can't be parameterized in MERGE → one batch per kind.
        let mut by_kind: HashMap<EdgeKind, Vec<&Edge>> = HashMap::new();
        for e in edges {
            by_kind.entry(e.kind).or_default().push(e);
        }
        for (kind, es) in &by_kind {
            let label = kind.cypher_label();
            let edge_chunks: Vec<_> = es.chunks(BATCH).collect();
            let total_edge_batches = edge_chunks.len();
            for (batch_idx, chunk) in edge_chunks.into_iter().enumerate() {
                let q = format!(
                    "UNWIND {arr} AS row \
                     MATCH (a:Symbol {{id: row.src}}), (b:Symbol {{id: row.dst}}) \
                     MERGE (a)-[r:{label}]->(b) \
                     ON CREATE SET r.confidence = row.conf, r.reason = row.reason, \
                         r.callSites = row.callSites",
                    arr = edges_to_list(chunk)
                );
                self.run_write(&q).await.inspect_err(|_| {
                    tracing::error!(
                        kind = ?kind,
                        batch = batch_idx,
                        committed_batches = batch_idx,
                        total_batches = total_edge_batches,
                        "edge batch write failed — graph is partially written; \
                         re-run bulk_load from scratch to restore consistency"
                    );
                })?;
            }
        }

        Ok(LoadStats {
            nodes: nodes.len() as u64,
            edges: edges.len() as u64,
        })
    }

    /// True when the graph holds no nodes — either it does not exist, or it is an
    /// empty graph key auto-created by a `GRAPH.QUERY` (e.g. the readiness probe
    /// in `wait_until_ready`, which is why an `EXISTS` check is not enough). Used
    /// to route `bulk_load` between the native `GRAPH.BULK` path and the Cypher
    /// upsert.
    async fn graph_is_empty(&self) -> Result<bool> {
        let rows = self
            .rows("MATCH (n) RETURN count(n) AS c")
            .await
            .unwrap_or_default();
        let count: u64 = rows
            .first()
            .and_then(|r| r.first())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Ok(count == 0)
    }

    /// Load a fresh graph via FalkorDB's native binary bulk-insert protocol
    /// (`GRAPH.BULK`). Skips the Cypher parser and per-edge `MATCH`; produces the
    /// same graph as the Cypher path (see `bulk::build_node_batches` /
    /// `build_edge_batches` for parity). Artifacts are **streamed** line-by-line
    /// and encoded into byte-budgeted batches — the node pass is sent and freed
    /// before the edge pass, so struct `Vec`s never exist and no single call
    /// approaches the 512 MB / 1 GB limits. The `id`/`kind` indexes are created
    /// *after* the insert — `BEGIN` requires an unused key.
    async fn bulk_insert(&self, artifacts: &GraphArtifacts) -> Result<LoadStats> {
        // `GRAPH.BULK BEGIN` requires an unused key. The readiness probe and the
        // emptiness check both auto-create an empty graph key, so drop it first.
        let _ = self.drop_graph().await;
        let budget = bulk::batch_budget();
        let read_err = |e: std::io::Error| GraphStoreError::Backend(format!("read artifacts: {e}"));

        // Node pass: stream + encode, keeping only the id→ordinal map.
        let t_load = std::time::Instant::now();
        let node_stream = artifacts.stream_nodes().map_err(read_err)?;
        let (node_batches, ordinal) =
            bulk::build_node_batches(node_stream, budget).map_err(read_err)?;
        let total_nodes = node_batches.iter().map(|(n, _)| n).sum::<u64>();
        if total_nodes == 0 {
            return Ok(LoadStats { nodes: 0, edges: 0 });
        }
        let mut payload_bytes: usize = node_batches.iter().map(|(_, b)| b.len()).sum();
        let mut call_count = node_batches.len();

        let mut con = {
            let _permit = self.acquire_permit().await?;
            let mut con = self.conn().await?;
            // Node batches first — they assign ordinals 0..N in send order.
            let mut first = true;
            for (nc, blob) in &node_batches {
                let mut cmd = redis::cmd("GRAPH.BULK");
                cmd.arg(&self.graph_key);
                if std::mem::take(&mut first) {
                    cmd.arg("BEGIN");
                }
                cmd.arg(*nc).arg(0).arg(1).arg(0).arg(blob.as_slice());
                let _reply: Value = cmd
                    .query_async(&mut con)
                    .await
                    .map_err(|e| GraphStoreError::Backend(format!("GRAPH.BULK nodes: {e}")))?;
            }
            con
        };
        drop(node_batches); // free node payload before encoding edges

        // Edge pass: stream + encode against the ordinal map, then send.
        let edge_stream = artifacts.stream_edges().map_err(read_err)?;
        let edge_batches =
            bulk::build_edge_batches(edge_stream, &ordinal, budget).map_err(read_err)?;
        let total_edges = edge_batches.iter().map(|(n, _)| n).sum::<u64>();
        payload_bytes += edge_batches.iter().map(|(_, b)| b.len()).sum::<usize>();
        call_count += edge_batches.len();
        {
            let _permit = self.acquire_permit().await?;
            for (ec, blob) in &edge_batches {
                let mut cmd = redis::cmd("GRAPH.BULK");
                cmd.arg(&self.graph_key)
                    .arg(0)
                    .arg(*ec)
                    .arg(0)
                    .arg(1)
                    .arg(blob.as_slice());
                let _reply: Value = cmd
                    .query_async(&mut con)
                    .await
                    .map_err(|e| GraphStoreError::Backend(format!("GRAPH.BULK edges: {e}")))?;
            }
        }
        let load_ms = t_load.elapsed().as_millis();

        // Indexes must be built after the bulk insert (see doc above).
        let t_idx = std::time::Instant::now();
        let _ = self.run("CREATE INDEX FOR (n:Symbol) ON (n.id)").await;
        let _ = self.run("CREATE INDEX FOR (n:Symbol) ON (n.kind)").await;
        let _ = self.run("CREATE INDEX FOR (n:Symbol) ON (n.name)").await;
        tracing::info!(
            nodes = total_nodes,
            edges = total_edges,
            payload_mb = payload_bytes / 1_048_576,
            calls = call_count,
            load_ms,
            index_ms = t_idx.elapsed().as_millis(),
            "falkor GRAPH.BULK load timings"
        );
        Ok(LoadStats {
            nodes: total_nodes,
            edges: total_edges,
        })
    }

    /// Handlers that serve `route`, found via the **inverse** of the stored
    /// handler→route `HANDLES_ROUTE` edge. Returns empty when `route` is not a
    /// route node (nothing `HANDLES_ROUTE`s *to* a method), so this doubles as a
    /// cheap route/non-route discriminator for `flow_downstream`.
    async fn route_handler_nodes(&self, route: &NodeId) -> Result<Vec<FlowNode>> {
        let q = format!(
            "CYPHER id={id} \
             MATCH (:Symbol {{id:$id}})<-[:HANDLES_ROUTE]-(h:Symbol) \
             RETURN h.id, h.kind, h.name, h.qualifiedName, h.file \
             ORDER BY h.name LIMIT 100",
            id = cstr(route.as_str())
        );
        let rows = self.rows(&q).await?;
        Ok(rows
            .iter()
            .filter(|r| r.len() >= 3)
            .map(|r| FlowNode {
                id: NodeId::new(r[0].clone()),
                kind: NodeKind::from_label(r[1].as_str()),
                name: r[2].clone(),
                qualified_name: r.get(3).filter(|s| !s.is_empty()).cloned(),
                file: r.get(4).cloned().unwrap_or_default(),
                depth: 1,
                parent_id: None,
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

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
