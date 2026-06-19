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
                  embedding vector({}),
                  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
                  PRIMARY KEY (node_id, chunk_idx)
                );
                CREATE INDEX IF NOT EXISTS cih_embeddings_node_id_idx
                  ON cih_embeddings (node_id);
                CREATE INDEX IF NOT EXISTS cih_embeddings_hnsw_idx
                  ON cih_embeddings USING hnsw (embedding vector_cosine_ops);
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
        let mut pending = Vec::new();

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
            }
        }

        if pending.is_empty() {
            return Ok(summary);
        }

        for batch in pending.chunks(64) {
            let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
            let embeddings = self.model.embed(&texts)?;
            for (chunk, embedding) in batch.iter().zip(embeddings) {
                self.upsert_chunk(chunk, embedding).await?;
                summary.chunks_embedded += 1;
            }
        }
        Ok(summary)
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
