# Plan: Embedding-Based Feature Clustering (`--feature-strategy embed`)

> Revised 2026-07-04 to match the current codebase. An earlier draft assumed a greenfield
> `EmbedStrategy` and an O(n²) all-pairs design; both were wrong for this repo. See
> "Current state" for what already exists.

## Context

`cih discover` classifies code into feature groups via `--feature-strategy`
(`package` | `structural` | `hybrid` | `llm`, defined in `cih-engine/src/discover.rs:17`).
These are path/keyword heuristics (brittle across modules) or LLM (expensive,
non-deterministic). Missing: a **deterministic, semantic** grouping that clusters classes by
meaning regardless of file location — e.g. `PaymentService` in `com.bank.core` and
`BillingController` in `com.bank.api` landing in one feature.

This plan adds `--feature-strategy embed`: build a **cosine k-NN similarity graph** from the
384-dim embeddings already stored by `cih embed` (pgvector + HNSW), then run the existing
Leiden algorithm on it. Deterministic (fixed seed), free (no LLM), reproducible.

### Current state (verified — read before implementing)

- **An `EmbedStrategy` already exists** at `crates/cih-grouping/src/strategies/embed.rs`, but
  it is a *residual filler*: it assigns leftover "shared" nodes to the nearest **existing**
  cluster centroid via cosine (`embed.rs:33`). It runs **inside `hybrid`** already
  (`cih-engine/src/feature_strategy.rs:102`). **Do not rename or repurpose it.** This plan
  adds a *separate, primary* clusterer under a new name — `EmbedClusterStrategy` — so the two
  coexist (residual-filler stays the hybrid step; the clusterer is the new standalone kind).
- **`cih-grouping` is Postgres-free by design** — it depends on an injected `Embedder` trait
  (`strategy.rs:17`), not `EmbedStore`. Keep it that way: the **engine** does the pgvector
  work and hands `cih-grouping` prebuilt cluster assignments; the grouping crate only
  names/emits. (This is the main architectural correction vs the old draft.)
- **Embeddings**: `cih_embeddings(node_id, chunk_idx, content_hash, embedding vector(384), PK(node_id,chunk_idx))`
  with an HNSW cosine index `cih_embeddings_hnsw_idx` (`cih-embed/src/store.rs:74-90`).
  Model is 384-dim (`model.rs:32`). A private `cosine_distance()` helper exists
  (`store.rs:358`) — reuse it. `get_node_embeddings` does **not** exist yet (new).
- **Artifacts**: discover writes a single `groups.jsonl` per version under
  `.cih/artifacts-features/<ver>/` (`cmd/features.rs:37`, `artifact.rs:8`). Emit into that —
  there is **no** `groups-embed.jsonl`. `FeatureGroupEntry.strategy` already documents `"embed"` (`entry.rs:13`).
- **Community graph** is already `petgraph::UnGraph<NodeId, f32>` (weighted) internally
  (`cih-community/src/lib.rs`), so weighted Leiden is feasible — it just isn't exposed publicly yet.

## Design: k-NN via HNSW, not O(n²)

For a banking-scale monolith (12k+ files), all-pairs similarity is a non-starter. Build the
graph with **approximate nearest neighbors** using an HNSW index in Postgres:

```
cih embed                         (prerequisite — stores per-chunk vectors + HNSW index)
    │  ENGINE refreshes per-NODE vectors after chunk upserts (see "ANN design" below)
    ├─ cih_node_vectors: one row per node = mean of its chunk vectors, own HNSW index  ← NEW
    │
cih discover --feature-strategy embed
    │  ENGINE (discover.rs) — owns Postgres + Leiden orchestration
    ├─ EmbedStore::node_vectors(node_ids)  ← NEW: HashMap<NodeId, NodeVec{vec,name,file,kind}>
    ├─ EmbedStore::knn_edges(node_ids, k, min_sim) ← NEW: ONE batched LATERAL query
    │     → similarity edges Vec<(NodeId, NodeId, f32=sim)>   (k≈15, sim>threshold 0.65)
    ├─ build UnGraph<NodeId, f32> (undirected; dedup (a,b) keeping max sim)
    ├─ cih_community::run_leiden_weighted(graph, cfg)  ← NEW public wrapper over existing Leiden
    │     → Vec<(NodeId, cluster_id)>
    │  cih-grouping (Postgres-free) — names + emits
    └─ EmbedClusterStrategy::label(clusters, node_meta, vectors) → Vec<FeatureGroupEntry>
           strategy="embed", confidence=mean intra-cluster sim, slug from label node
    │
.cih/artifacts-features/<ver>/groups.jsonl   (single file; merged with overrides as today)
```

The clustered universe is exactly the nodes `cih embed` embedded (`embeddable_nodes` in
`cih-embed/src/store.rs`) — no separate node-kind filter to maintain here, and no node-dropping cap.

### ANN design (resolved): per-node materialized table, single batched query

**Problem.** The HNSW index (`cih_embeddings_hnsw_idx`) is built on **per-chunk** rows
(`PK(node_id, chunk_idx)`), but clustering happens at **per-node** granularity. Averaging chunks
in memory and then probing the chunk-level index means querying with a vector that matches no
indexed row (recall degrades); per-node dedup of chunk hits changes what "neighbor" means and
multiplies round-trips. Resolve the mismatch in storage, not at query time.

**1. New materialized table `cih_node_vectors`** (added in `ensure_schema`, alongside the
existing chunk table):

```sql
CREATE TABLE IF NOT EXISTS cih_node_vectors (
  node_id     TEXT PRIMARY KEY,
  node_kind   TEXT NOT NULL,
  name        TEXT NOT NULL,
  file        TEXT NOT NULL,
  embedding   vector(384) NOT NULL,          -- mean of this node's chunk vectors
  chunk_count INTEGER NOT NULL,
  updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS cih_node_vectors_hnsw_idx
  ON cih_node_vectors USING hnsw (embedding vector_cosine_ops);
```

The HNSW index now sits at the exact granularity we cluster at, so k-NN queries hit indexed
rows. It also carries `node_kind/name/file`, so `node_vectors` returns node metadata in the same
query — the engine needs no second source, and the `cih-grouping` crate stays Postgres-free
(engine passes it in). `<=>` is cosine, magnitude-invariant, so storing the raw mean (not
L2-normalized) is fine; the in-memory centroid/label step reuses `cosine_distance`
(`store.rs:358`), which also cancels magnitude.

**2. Refresh after chunk upserts** — at the end of `embed_nodes`, aggregate with pgvector's
`avg(vector)` (available since pgvector 0.5, which HNSW already requires), scoped to the node_ids
whose chunks changed this run (the `pending` set) so incrementality holds:

```sql
INSERT INTO cih_node_vectors (node_id, node_kind, name, file, embedding, chunk_count, updated_at)
SELECT node_id, min(node_kind), min(name), min(file),   -- constant per node_id; min() satisfies GROUP BY
       avg(embedding)::vector(384), count(*), now()
FROM cih_embeddings
WHERE node_id = ANY($1)
GROUP BY node_id
ON CONFLICT (node_id) DO UPDATE SET
  node_kind = EXCLUDED.node_kind, name = EXCLUDED.name, file = EXCLUDED.file,
  embedding = EXCLUDED.embedding, chunk_count = EXCLUDED.chunk_count, updated_at = now();
```

Then **prune to the current node set in the same pass** so `cih_node_vectors` == the graph
`cih embed` just saw — this is what lets the k-NN query below run *without* an inner filter (see
step 3). Key the delete off the **current embeddable node_ids** (`$1`), *not* "has any chunk":
`cih_embeddings` keeps orphan chunk rows for renamed/deleted classes until a future
`cih embed --prune`, so a "no chunks" test would leave those orphans in the node table.

```sql
DELETE FROM cih_node_vectors WHERE node_id <> ALL($1);   -- $1 = all current embeddable node_ids
```

This is safe because `embed_nodes` is always invoked over the **full** current node set (it must
see every node to skip-by-hash), so `$1` is authoritative. `embeddable_nodes(nodes)` already
gives this list.

**3. Batched k-NN via one LATERAL query** — no per-node round-trips. pgvector uses the HNSW
index for the correlated `ORDER BY embedding <=> q.embedding LIMIT k`:

```sql
SELECT q.node_id AS src, nbr.node_id AS dst, (1.0 - (q.embedding <=> nbr.embedding))::real AS sim
FROM cih_node_vectors q
CROSS JOIN LATERAL (
    SELECT n.node_id, n.embedding
    FROM cih_node_vectors n
    WHERE n.node_id <> q.node_id
    ORDER BY n.embedding <=> q.embedding, n.node_id   -- node_id tiebreak = deterministic
    LIMIT $1                                           -- k
) nbr
WHERE (q.embedding <=> nbr.embedding) <= $2;           -- 1 - min_sim
```

**No `node_id = ANY(...)` filter inside the LATERAL — this is deliberate.** pgvector's HNSW
index *post-filters*: a `WHERE` predicate is applied **after** the top-k index scan, so a filtered
ANN can return **fewer than k** in-graph neighbors (recall loss), not merely run slower. Rather
than fight that with over-fetching, we keep the table itself clean via the prune in step 2, so
every row is already in-graph and no inner filter is needed. (The prior `analyze` step defines the
current node set; because embed → discover run against the same version, the pruned table and the
discover node set match. As a defensive backstop, the engine still drops any edge whose endpoint
isn't in the discover node set when building the `UnGraph`.)

k-NN is asymmetric; the engine builds the `UnGraph` by inserting each edge as
`(min(a,b), max(a,b))` and keeping the **max** sim on collision, yielding a stable undirected
weighted graph. For small indexes the planner falls back to a seq-scan of the same query (exact,
perfect recall) — mirroring the existing `semantic_search` N≤2000 exact-scan split; no
special-casing needed in Rust.

Complexity ≈ `O(n · k · log n)` inside one round trip (n index-backed LATERAL probes), scales to
the whole graph. The earlier "n separate HNSW queries" concern is gone — it is a single
statement.

## Files to modify

1. **`crates/cih-embed/src/store.rs`** — the per-node vector layer (see "ANN design" above):
   - `ensure_schema`: add the `cih_node_vectors` table + `cih_node_vectors_hnsw_idx`.
   - `embed_nodes`: after the chunk-upsert loop (before `Ok(summary)`), refresh
     `cih_node_vectors` for the changed node_ids via the `avg(embedding)` upsert, then run the
     orphan `DELETE`. (Keeps incrementality — only touched nodes re-aggregate — and keeps the
     table == current node set. `pending` is local to `embed_nodes`, so collect its distinct
     `node_id`s inline; no signature change.)
   - `node_vectors(&self, node_ids) -> HashMap<NodeId, NodeVec>` where
     `NodeVec { vec: Vec<f32>, node_kind, name, file }` — one `SELECT ... FROM cih_node_vectors
     WHERE node_id = ANY($1)`. Returns vectors **and** metadata so the engine has no second
     source and `cih-grouping` stays DB-free.
   - `knn_edges(&self, k, min_sim) -> Vec<(NodeId, NodeId, f32)>` — the single batched LATERAL
     query above (no node-id filter; the pruned table *is* the current set). Returns similarity
     edges directly. Reuse `cosine_distance` (`:358`) only for the in-memory centroid/label step
     in `EmbedClusterStrategy`, not for k-NN.

2. **`crates/cih-community/src/lib.rs`** — expose the weighted-graph Leiden that
   `detect_communities` already runs internally:
   `pub fn run_leiden_weighted(graph: UnGraph<NodeId, f32>, cfg: &CommunityConfig) -> Vec<(NodeId, usize)>`.
   Thin extraction of the existing invocation — verify the seed is threaded for determinism.

3. **`crates/cih-grouping/src/strategies/embed_cluster.rs`** — NEW (distinct from `embed.rs`):
   `EmbedClusterStrategy` + `EmbedClusterConfig { similarity_threshold: 0.65, knn: 15, leiden_resolution: 0.8, leiden_seed: 0xc0de }`.
   Input is **precomputed cluster assignments + node metadata + per-node vectors** (no DB, no
   Leiden here). It: computes each cluster's centroid, picks the label node (min cosine
   distance to centroid), derives a slug (non-generic module dir → stripped class name
   [`Controller`/`Service`/`Repository`/`Handler`/`Impl`/`Manager`] → 2nd-to-last package
   segment → `cluster-{n}`), and emits `FeatureGroupEntry{ strategy:"embed", confidence:sim_to_centroid, evidence:"knn-leiden k=.. thr=.. res=.. label=.." }`.
   Export from `lib.rs`. Add `embed_cluster_tests.rs` mirroring `embed_tests.rs`.

4. **`crates/cih-engine/src/discover.rs`** — orchestration:
   - Add `Embed` to `FeatureStrategyKind` (:17) + its `Display`/`FromStr` arms.
   - `run_discover` is currently sync; the store methods are async. Wrap the Embed arm in a
     runtime-safe island rather than making the whole path async. **Do not** use a bare
     `Runtime::new().block_on()` — that panics if `run_discover` is ever driven from inside an
     existing tokio runtime (e.g. the MCP server invoking discover). Use
     `tokio::task::block_in_place(|| Handle::current().block_on(fut))` when a runtime is present,
     falling back to a fresh `Runtime` only when there is no current handle (CLI path). Today
     discover is sync/CLI-only so either works, but this keeps the async boundary safe for the
     server integration.
   - When kind==Embed (or Hybrid opts in), require `pg_url`; fetch `node_vectors` +
     `knn_edges`, build the `UnGraph` (dedup keeping max sim), call `run_leiden_weighted`, pass
     assignments + `node_vectors` metadata to `EmbedClusterStrategy`.

5. **`crates/cih-engine/src/feature_strategy.rs`** — add the `Embed` build arm (returns the
   naming step for `EmbedClusterStrategy`); leave the existing `EmbedStrategy` residual step
   in the `Hybrid` arm unchanged.

6. **`crates/cih-engine/src/settings.rs`** + **`main.rs`** — this session's config layer:
   accept `embed` in `feature_strategy`, and add `[discover] embed_similarity_threshold`,
   `embed_knn`, `embed_leiden_resolution` to `DiscoverSettings` with resolver defaults, so the
   knobs are persistable in `cih.toml` (not just flags). Add matching `--embed-*` flags as `Option<T>`.
   `DiscoverSettings` is `#[serde(default, deny_unknown_fields)]` (`settings.rs:118`), so adding
   the fields is necessary **but not sufficient** — also thread them through:
   - `effective_rows()` (`settings.rs:219`) — else `cih config show` won't list the new knobs.
   - `starter_toml()` (`settings.rs:311`) — else `cih config init` won't scaffold them.
   Miss either and the options work but are invisible to the config UX; miss the field on the
   struct and `deny_unknown_fields` makes a `cih.toml` that sets them fail to parse.

## Coexistence with the existing residual `EmbedStrategy`

- `--feature-strategy embed` → new `EmbedClusterStrategy` (primary, from-scratch clustering).
- `--feature-strategy hybrid` → unchanged: still uses the existing residual `EmbedStrategy`
  (`embed.rs`) as its embed step. Optionally, a later iteration can let hybrid seed from
  cluster output, but that's out of scope here.

## Incremental updates

Clustering is as fresh as the last `cih embed` (BLAKE3 `content_hash` → only changed chunks
re-embed; a 5–20 file PR is seconds). The `cih_node_vectors` refresh re-aggregates only node_ids
whose chunks changed this run, and its paired `DELETE ... WHERE node_id <> ALL(current_node_ids)`
drops rows for classes gone from the current graph — so `cih_node_vectors` stays == the current
node set and `knn_edges` needs no per-query filter. (`cih_embeddings` itself may still retain
orphan chunk rows until a future `cih embed --prune`; that doesn't affect clustering, since edges
come from the node table, which is pruned every run.) Pipeline:
`analyze → embed → discover --feature-strategy embed`.

## Prerequisites & degradation

| Condition | Behavior |
|---|---|
| `cih embed` never run / `--pg-url` unset | Error with "run `cih embed --pg-url` first"; or fall back to `structural` with a warning |
| Embeddings for < 50% of nodes | Proceed with warning; unembedded nodes → `"shared"` |
| No edges above threshold | Warn "lower `--embed-similarity-threshold`"; each node → its own package slug |

## Caveats (state honestly in the docs)

- **Requires Postgres/pgvector + a prior `cih embed`** — unlike `package`/`structural`, this
  strategy is not self-contained.
- **Determinism** holds only with a fixed Leiden seed *and* stable iteration order; ANN recall
  and float summation can perturb ties — document as "near-deterministic."

## New dependencies

None. `cih-embed` already uses `tokio-postgres`/pgvector; `cih-community` already uses
`petgraph`/Leiden; naming reuses `cosine_distance`.

## Verification

1. `cargo test --workspace` — no regressions; new `embed_cluster_tests.rs` green.
2. `cih embed --pg-url $PG_URL` then `cih discover --feature-strategy embed --pg-url $PG_URL`.
3. Inspect `.cih/artifacts-features/<ver>/groups.jsonl`: entries with `"strategy":"embed"`,
   meaningful slugs (not all `cluster-0`), most nodes assigned (not all `"shared"`).
4. Diff against `--feature-strategy package`: embed should merge cross-package domain nodes.
5. Determinism: run twice, assert **stable** (not bit-identical) membership — Jaccard ≥ 0.95
   per cluster. HNSW ANN recall + float summation perturb ties, so exact identity is the wrong
   assertion (matches the "near-deterministic" caveat above).
6. Degradation: run without `--pg-url` → clean fallback/error, no panic.
7. Scale: on the target monolith, confirm no node-dropping and end-to-end within a few seconds
   over the ANN path.
