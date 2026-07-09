use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use cih_core::{Node, NodeId, NodeKind, Range};
use pgvector::Vector;
use serde::{Deserialize, Serialize};
use tokio_postgres::{Client, NoTls};

use crate::chunk_text;
use crate::model::{EmbedModel, EmbedModelKind};
use crate::text::{content_hash, embeddable_nodes, embedding_text, source_bodies};

const CHUNK_BYTES: usize = 4_000;
const OVERLAP_BYTES: usize = 500;
/// Chunks embedded + upserted per flush. Bounded so `embed_nodes` never holds the whole repo's
/// chunk texts in memory (multiple GB at 600k nodes); also amortizes the model call.
const EMBED_BATCH: usize = 256;

pub struct EmbedStore {
    client: Client,
    model: EmbedModel,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EmbedSummary {
    pub nodes_considered: usize,
    pub chunks_total: usize,
    pub chunks_embedded: usize,
    pub chunks_skipped: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SemanticHit {
    pub node_id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub file: String,
    pub range: Range,
    pub distance: f32,
    pub score: f32,
}

/// A per-node averaged embedding plus its metadata, read from `cih_node_vectors`.
/// Used by feature clustering (`cih discover --feature-strategy embed`).
#[derive(Clone, Debug)]
pub struct NodeVector {
    pub node_id: NodeId,
    pub node_kind: String,
    pub name: String,
    pub file: String,
    pub vector: Vec<f32>,
}

#[derive(Clone, Debug)]
struct PendingChunk {
    node_id: String,
    kind: NodeKind,
    name: String,
    file: String,
    chunk_idx: i32,
    start_line: i32,
    end_line: i32,
    text: String,
    hash: String,
}

impl EmbedStore {
    pub async fn connect(pg_url: &str, model_kind: EmbedModelKind) -> Result<Self> {
        let (client, connection) = tokio_postgres::connect(pg_url, NoTls)
            .await
            .with_context(|| "failed to connect to Postgres for embeddings")?;
        tokio::spawn(async move {
            if let Err(err) = connection.await {
                eprintln!("cih-embed postgres connection error: {err}");
            }
        });
        let model = EmbedModel::load(model_kind)
            .with_context(|| format!("failed to load embedding model {}", model_kind.label()))?;
        Ok(Self { client, model })
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        self.client
            .batch_execute(&format!(
                r#"
                CREATE EXTENSION IF NOT EXISTS vector;
                CREATE TABLE IF NOT EXISTS cih_embeddings (
                  node_id TEXT NOT NULL,
                  chunk_idx INTEGER NOT NULL,
                  node_kind TEXT NOT NULL,
                  name TEXT NOT NULL,
                  file TEXT NOT NULL,
                  start_line INTEGER NOT NULL,
                  end_line INTEGER NOT NULL,
                  content_hash TEXT NOT NULL,
                  embedding vector({0}),
                  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                  PRIMARY KEY (node_id, chunk_idx)
                );
                CREATE INDEX IF NOT EXISTS cih_embeddings_node_id_idx
                  ON cih_embeddings (node_id);
                CREATE INDEX IF NOT EXISTS cih_embeddings_hnsw_idx
                  ON cih_embeddings USING hnsw (embedding vector_cosine_ops);
                -- Per-node materialized vectors: the mean of a node's chunk vectors, with its
                -- own HNSW index. Feature clustering (`cih discover --feature-strategy embed`)
                -- runs k-NN at node granularity against this table, not the per-chunk table.
                CREATE TABLE IF NOT EXISTS cih_node_vectors (
                  node_id     TEXT PRIMARY KEY,
                  node_kind   TEXT NOT NULL,
                  name        TEXT NOT NULL,
                  file        TEXT NOT NULL,
                  embedding   vector({0}) NOT NULL,
                  chunk_count INTEGER NOT NULL,
                  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
                );
                CREATE INDEX IF NOT EXISTS cih_node_vectors_hnsw_idx
                  ON cih_node_vectors USING hnsw (embedding vector_cosine_ops);
                "#,
                self.model.dimension()
            ))
            .await
            .with_context(|| "failed to ensure cih_embeddings schema")?;
        Ok(())
    }

    pub async fn embed_nodes(&self, nodes: &[Node], repo: &Path) -> Result<EmbedSummary> {
        let bodies = source_bodies(nodes, repo);
        let embeddable = embeddable_nodes(nodes);
        let node_ids: Vec<String> = embeddable
            .iter()
            .map(|node| node.id.as_str().to_string())
            .collect();
        let existing = self.existing_chunk_hashes(&node_ids).await?;

        let mut summary = EmbedSummary {
            nodes_considered: embeddable.len(),
            ..EmbedSummary::default()
        };
        // Embed in bounded batches, flushing (embed + upsert) as we go so we never hold every
        // chunk's text in memory — at 600k nodes that would be several GB. Track only the small
        // set of changed node ids for the per-node vector refresh.
        let mut pending: Vec<PendingChunk> = Vec::with_capacity(EMBED_BATCH);
        let mut changed: HashSet<String> = HashSet::new();

        for node in embeddable {
            let body = bodies.get(node.id.as_str()).map(|s| s.as_str());
            let text = embedding_text(node, body);
            for chunk in chunk_text(&text, CHUNK_BYTES, OVERLAP_BYTES) {
                summary.chunks_total += 1;
                let hash = content_hash(node.id.as_str(), &chunk.text);
                let key = (node.id.as_str().to_string(), chunk.chunk_idx as i32);
                if existing.get(&key).is_some_and(|seen| seen == &hash) {
                    summary.chunks_skipped += 1;
                    continue;
                }
                changed.insert(node.id.as_str().to_string());
                // Use the node's actual source file lines, not the chunk's line
                // position within the embedding text string.
                pending.push(PendingChunk {
                    node_id: node.id.as_str().to_string(),
                    kind: node.kind,
                    name: node.name.clone(),
                    file: node.file.clone(),
                    chunk_idx: chunk.chunk_idx as i32,
                    start_line: node.range.start_line as i32,
                    end_line: node.range.end_line as i32,
                    text: chunk.text,
                    hash,
                });
                if pending.len() >= EMBED_BATCH {
                    self.embed_and_upsert(&pending, &mut summary).await?;
                    pending.clear();
                }
            }
        }
        self.embed_and_upsert(&pending, &mut summary).await?;

        // Refresh the per-node vector table. Re-aggregate only nodes whose chunks changed
        // this run (`changed`), then prune the table to the current embeddable set (`node_ids`)
        // so it always equals the current graph — this is what lets `knn_edges` run without a
        // per-query node filter (an HNSW post-filter would drop it below k). The prune keys off
        // the current node set (not "has any chunk"), because `cih_embeddings` may still hold
        // orphan chunk rows for renamed/deleted classes until a future `cih embed --prune`.
        let mut changed: Vec<String> = changed.into_iter().collect();
        changed.sort();
        self.upsert_node_vectors(&changed).await?;
        self.prune_node_vectors(&node_ids).await?;
        Ok(summary)
    }

    /// Embed one batch of chunks and upsert them. Kept separate so `embed_nodes` can flush
    /// incrementally without accumulating the whole repo's chunk texts in memory.
    async fn embed_and_upsert(
        &self,
        batch: &[PendingChunk],
        summary: &mut EmbedSummary,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
        let embeddings = self.model.embed(&texts)?;
        for (chunk, embedding) in batch.iter().zip(embeddings) {
            self.upsert_chunk(chunk, embedding).await?;
            summary.chunks_embedded += 1;
        }
        Ok(())
    }

    /// Re-aggregate `cih_node_vectors` rows for `node_ids` as the mean of their chunk vectors.
    /// No-op for ids with no chunk rows. Uses pgvector's `avg(vector)` aggregate.
    pub async fn upsert_node_vectors(&self, node_ids: &[String]) -> Result<u64> {
        if node_ids.is_empty() {
            return Ok(0);
        }
        let n = self
            .client
            .execute(
                r#"
                INSERT INTO cih_node_vectors
                  (node_id, node_kind, name, file, embedding, chunk_count, updated_at)
                SELECT node_id, min(node_kind), min(name), min(file),
                       avg(embedding)::vector, count(*)::int, now()
                FROM cih_embeddings
                WHERE node_id = ANY($1)
                GROUP BY node_id
                ON CONFLICT (node_id) DO UPDATE SET
                  node_kind = EXCLUDED.node_kind,
                  name = EXCLUDED.name,
                  file = EXCLUDED.file,
                  embedding = EXCLUDED.embedding,
                  chunk_count = EXCLUDED.chunk_count,
                  updated_at = now()
                "#,
                &[&node_ids],
            )
            .await
            .with_context(|| "failed to refresh cih_node_vectors")?;
        Ok(n)
    }

    /// Drop `cih_node_vectors` rows whose node_id is not in `keep` (the current graph's node
    /// set), so the table always equals the current graph.
    pub async fn prune_node_vectors(&self, keep: &[String]) -> Result<u64> {
        // Empty `keep` would delete everything; treat as "nothing current" only when the caller
        // genuinely passes an empty graph. Guard against accidental wipe of a populated table.
        if keep.is_empty() {
            return Ok(0);
        }
        let n = self
            .client
            .execute(
                "DELETE FROM cih_node_vectors WHERE node_id <> ALL($1)",
                &[&keep],
            )
            .await
            .with_context(|| "failed to prune cih_node_vectors")?;
        Ok(n)
    }

    /// Number of rows in `cih_node_vectors` (used to detect an un-populated table so discover
    /// can self-heal against an older `cih embed` that predates this table).
    pub async fn node_vector_count(&self) -> Result<i64> {
        let count: i64 = self
            .client
            .query_one("SELECT COUNT(*) FROM cih_node_vectors", &[])
            .await?
            .get(0);
        Ok(count)
    }

    /// Fetch per-node vectors + metadata for `node_ids` from `cih_node_vectors`.
    pub async fn node_vectors(&self, node_ids: &[String]) -> Result<Vec<NodeVector>> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self
            .client
            .query(
                r#"
                SELECT node_id, node_kind, name, file, embedding
                FROM cih_node_vectors
                WHERE node_id = ANY($1)
                "#,
                &[&node_ids],
            )
            .await
            .with_context(|| "failed to read cih_node_vectors")?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let vector: Vector = row.get(4);
                NodeVector {
                    node_id: NodeId::new(row.get::<_, String>(0)),
                    node_kind: row.get(1),
                    name: row.get(2),
                    file: row.get(3),
                    vector: vector.to_vec(),
                }
            })
            .collect())
    }

    /// Build the cosine k-NN similarity graph over `cih_node_vectors` in one batched query.
    /// Returns `(src, dst, similarity)` edges with `similarity >= min_sim`, up to `k` neighbors
    /// per node.
    ///
    /// The inner LATERAL is the **pure** `ORDER BY embedding <=> q LIMIT` form so the cosine HNSW
    /// index (`cih_node_vectors_hnsw_idx`) is used. A compound `ORDER BY` (e.g. a `node_id`
    /// tiebreak) silently disables the index and forces an exact O(N²) scan — ~46 min on a
    /// 77k-vector graph. Self-exclusion and the similarity threshold are applied **outside** the
    /// LATERAL, and we fetch `k+1` so dropping the self-match still leaves `k` neighbors.
    ///
    /// HNSW is approximate vs a brute-force scan, but deterministic for a fixed index +
    /// `ef_search` (the randomness is at index-build time), so clustering stays reproducible.
    pub async fn knn_edges(&self, k: usize, min_sim: f32) -> Result<Vec<(NodeId, NodeId, f32)>> {
        let max_distance = (1.0 - min_sim) as f64;
        // Fetch one extra neighbor to absorb the self-match we filter out below.
        let limit = (k + 1) as i64;
        // Recall knob for the HNSW traversal; must be >= LIMIT. Higher = better recall, more work.
        let ef_search = (4 * (k + 1)).max(64);
        self.client
            .batch_execute(&format!("SET hnsw.ef_search = {ef_search}"))
            .await
            .with_context(|| "failed to set hnsw.ef_search")?;
        let rows = self
            .client
            .query(
                r#"
                SELECT q.node_id AS src,
                       nbr.node_id AS dst,
                       (1.0 - (q.embedding <=> nbr.embedding))::real AS sim
                FROM cih_node_vectors q
                CROSS JOIN LATERAL (
                    SELECT n.node_id, n.embedding
                    FROM cih_node_vectors n
                    ORDER BY n.embedding <=> q.embedding
                    LIMIT $1
                ) nbr
                WHERE nbr.node_id <> q.node_id
                  AND (q.embedding <=> nbr.embedding) <= $2
                "#,
                &[&limit, &max_distance],
            )
            .await
            .with_context(|| "failed to run k-NN query over cih_node_vectors")?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    NodeId::new(row.get::<_, String>(0)),
                    NodeId::new(row.get::<_, String>(1)),
                    row.get::<_, f32>(2),
                )
            })
            .collect())
    }

    /// Streaming variant of [`knn_edges`]: invokes `on_edge(src, dst, sim)` per row without ever
    /// materializing the full result (9M+ rows on a 600k-node graph). The caller interns ids to a
    /// compact index inside the closure, so peak memory stays flat regardless of edge count. `src`
    /// and `dst` borrow the row and are only valid for the callback.
    pub async fn knn_edges_streamed<F>(&self, k: usize, min_sim: f32, mut on_edge: F) -> Result<()>
    where
        F: FnMut(&str, &str, f32),
    {
        use futures_util::TryStreamExt;
        use tokio_postgres::types::ToSql;

        let max_distance = (1.0 - min_sim) as f64;
        let limit = (k + 1) as i64;
        let ef_search = (4 * (k + 1)).max(64);
        self.client
            .batch_execute(&format!("SET hnsw.ef_search = {ef_search}"))
            .await
            .with_context(|| "failed to set hnsw.ef_search")?;

        let params: Vec<&(dyn ToSql + Sync)> = vec![&limit, &max_distance];
        let stream = self
            .client
            .query_raw(
                r#"
                SELECT q.node_id AS src,
                       nbr.node_id AS dst,
                       (1.0 - (q.embedding <=> nbr.embedding))::real AS sim
                FROM cih_node_vectors q
                CROSS JOIN LATERAL (
                    SELECT n.node_id, n.embedding
                    FROM cih_node_vectors n
                    ORDER BY n.embedding <=> q.embedding
                    LIMIT $1
                ) nbr
                WHERE nbr.node_id <> q.node_id
                  AND (q.embedding <=> nbr.embedding) <= $2
                "#,
                params,
            )
            .await
            .with_context(|| "failed to start streamed k-NN query over cih_node_vectors")?;
        futures_util::pin_mut!(stream);
        while let Some(row) = stream
            .try_next()
            .await
            .with_context(|| "k-NN row stream error")?
        {
            let src: &str = row.get(0);
            let dst: &str = row.get(1);
            let sim: f32 = row.get(2);
            on_edge(src, dst, sim);
        }
        Ok(())
    }

    /// Compute each node's cosine similarity to its cluster centroid **in Postgres** — so the 600k
    /// per-node vectors never enter Rust. Given `(node_id, cluster_id)` assignments (as parallel
    /// arrays), returns `(node_id, node_kind, name, file, sim)` for each assigned node. Centroids
    /// are the mean embedding per cluster (pgvector `avg`), sim is `1 - cosine_distance`.
    pub async fn node_confidences(
        &self,
        node_ids: &[String],
        cluster_ids: &[i32],
    ) -> Result<Vec<(String, String, String, String, f32)>> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self
            .client
            .query(
                r#"
                WITH asn(node_id, cluster_id) AS (
                    SELECT * FROM unnest($1::text[], $2::int[])
                ),
                cent AS (
                    SELECT a.cluster_id, avg(v.embedding)::vector AS centroid
                    FROM asn a JOIN cih_node_vectors v USING (node_id)
                    GROUP BY a.cluster_id
                )
                SELECT v.node_id, v.node_kind, v.name, v.file,
                       (1.0 - (v.embedding <=> c.centroid))::real AS sim
                FROM asn a
                JOIN cih_node_vectors v USING (node_id)
                JOIN cent c USING (cluster_id)
                "#,
                &[&node_ids, &cluster_ids],
            )
            .await
            .with_context(|| "failed to compute per-node confidences in Postgres")?;
        Ok(rows
            .into_iter()
            .map(|r| {
                (
                    r.get::<_, String>(0),
                    r.get::<_, String>(1),
                    r.get::<_, String>(2),
                    r.get::<_, String>(3),
                    r.get::<_, f32>(4),
                )
            })
            .collect())
    }

    pub async fn semantic_search(
        &self,
        query: &str,
        limit: usize,
        max_distance: f32,
    ) -> Result<Vec<SemanticHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut query_embeddings = self.model.embed(&[query.to_string()])?;
        let Some(query_embedding) = query_embeddings.pop() else {
            return Ok(Vec::new());
        };

        let count: i64 = self
            .client
            .query_one("SELECT COUNT(*) FROM cih_embeddings", &[])
            .await?
            .get(0);
        // For small local indexes, exact scan avoids ANN index warm-up/recall tradeoffs.
        if count <= 2_000 {
            return self
                .exact_search(&query_embedding, limit, max_distance)
                .await;
        }

        let ann = self
            .ann_search(&query_embedding, limit, max_distance)
            .await?;
        if ann.is_empty() {
            self.exact_search(&query_embedding, limit, max_distance)
                .await
        } else {
            Ok(ann)
        }
    }

    async fn existing_chunk_hashes(
        &self,
        node_ids: &[String],
    ) -> Result<HashMap<(String, i32), String>> {
        if node_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = self
            .client
            .query(
                "SELECT node_id, chunk_idx, content_hash FROM cih_embeddings WHERE node_id = ANY($1)",
                &[&node_ids],
            )
            .await?;
        let mut hashes = HashMap::new();
        for row in rows {
            let node_id: String = row.get(0);
            let chunk_idx: i32 = row.get(1);
            let content_hash: String = row.get(2);
            hashes.insert((node_id, chunk_idx), content_hash);
        }
        Ok(hashes)
    }

    async fn upsert_chunk(&self, chunk: &PendingChunk, embedding: Vec<f32>) -> Result<()> {
        let vector = Vector::from(embedding);
        self.client
            .execute(
                r#"
                INSERT INTO cih_embeddings
                  (node_id, chunk_idx, node_kind, name, file, start_line, end_line, content_hash, embedding, updated_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
                ON CONFLICT (node_id, chunk_idx) DO UPDATE SET
                  node_kind = EXCLUDED.node_kind,
                  name = EXCLUDED.name,
                  file = EXCLUDED.file,
                  start_line = EXCLUDED.start_line,
                  end_line = EXCLUDED.end_line,
                  content_hash = EXCLUDED.content_hash,
                  embedding = EXCLUDED.embedding,
                  updated_at = now()
                "#,
                &[
                    &chunk.node_id,
                    &chunk.chunk_idx,
                    &chunk.kind.label(),
                    &chunk.name,
                    &chunk.file,
                    &chunk.start_line,
                    &chunk.end_line,
                    &chunk.hash,
                    &vector,
                ],
            )
            .await?;
        Ok(())
    }

    async fn ann_search(
        &self,
        query_embedding: &[f32],
        limit: usize,
        max_distance: f32,
    ) -> Result<Vec<SemanticHit>> {
        let vector = Vector::from(query_embedding.to_vec());
        let row_limit = (limit * 4).max(limit) as i64;
        let rows = self
            .client
            .query(
                r#"
                SELECT node_id, node_kind, name, file, start_line, end_line,
                       (embedding <=> $1) AS distance
                FROM cih_embeddings
                WHERE (embedding <=> $1) <= $2
                ORDER BY embedding <=> $1
                LIMIT $3
                "#,
                &[&vector, &(max_distance as f64), &row_limit],
            )
            .await?;
        Ok(dedupe_hits(rows.into_iter().map(row_to_hit), limit))
    }

    async fn exact_search(
        &self,
        query_embedding: &[f32],
        limit: usize,
        max_distance: f32,
    ) -> Result<Vec<SemanticHit>> {
        let rows = self
            .client
            .query(
                r#"
                SELECT node_id, node_kind, name, file, start_line, end_line, embedding
                FROM cih_embeddings
                LIMIT 50000
                "#,
                &[],
            )
            .await?;
        let mut hits = Vec::new();
        for row in rows {
            let vector: Vector = row.get(6);
            let distance = cosine_distance(query_embedding, &vector.to_vec());
            if distance <= max_distance {
                hits.push(SemanticHit {
                    node_id: NodeId::new(row.get::<_, String>(0)),
                    kind: NodeKind::from_label(&row.get::<_, String>(1)),
                    name: row.get(2),
                    file: row.get(3),
                    range: Range {
                        start_line: row.get::<_, i32>(4) as u32,
                        start_col: 0,
                        end_line: row.get::<_, i32>(5) as u32,
                        end_col: 0,
                    },
                    distance,
                    score: 1.0 - distance,
                });
            }
        }
        hits.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.node_id.as_str().cmp(b.node_id.as_str()))
        });
        Ok(dedupe_hits(hits, limit))
    }
}

fn row_to_hit(row: tokio_postgres::Row) -> SemanticHit {
    let distance = row.get::<_, f64>(6) as f32;
    SemanticHit {
        node_id: NodeId::new(row.get::<_, String>(0)),
        kind: NodeKind::from_label(&row.get::<_, String>(1)),
        name: row.get(2),
        file: row.get(3),
        range: Range {
            start_line: row.get::<_, i32>(4) as u32,
            start_col: 0,
            end_line: row.get::<_, i32>(5) as u32,
            end_col: 0,
        },
        distance,
        score: 1.0 - distance,
    }
}

fn dedupe_hits<I>(hits: I, limit: usize) -> Vec<SemanticHit>
where
    I: IntoIterator<Item = SemanticHit>,
{
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for hit in hits {
        if seen.insert(hit.node_id.clone()) {
            deduped.push(hit);
            if deduped.len() == limit {
                break;
            }
        }
    }
    deduped
}

fn cosine_distance(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 1.0;
    }
    let mut dot = 0.0;
    let mut a_norm = 0.0;
    let mut b_norm = 0.0;
    for (left, right) in a.iter().zip(b.iter()) {
        dot += left * right;
        a_norm += left * left;
        b_norm += right * right;
    }
    if a_norm == 0.0 || b_norm == 0.0 {
        1.0
    } else {
        1.0 - dot / (a_norm.sqrt() * b_norm.sqrt())
    }
}
