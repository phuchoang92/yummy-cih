# Phase 6 — Search: BM25 + Embeddings + Hybrid (detailed plan)

Goal: make the graph queryable by natural language — an in-memory BM25 index over node names plus a
semantic vector index in pgvector, merged via Reciprocal Rank Fusion (RRF k=60) to power the `query`
MCP tool. Acceptance: `query("user registration")` returns ranked, relevant code symbols.

Builds on Phase 5: the `GraphStore::subgraph(seeds, radius)` port (for result expansion) and the
JSONL artifacts from Phase 3–4 (for BM25 input). Ports constants and the merge algorithm from
GitNexus (`src/core/search/hybrid-search.ts`); porting character chunking from
`src/core/embeddings/character-chunk.ts`. BM25 is implemented from scratch in Rust (the GitNexus
`bm25-index.ts` delegates to LadybugDB's built-in FTS, which does not exist in FalkorDB).

New crates: `cih-search` (BM25 + RRF) and `cih-embed` (chunking + fastembed + pgvector). Engine
gains an `embed` subcommand. MCP server gains the `query` tool.

---

## What already exists (do not re-implement)

| Item | Location | Status |
|------|----------|--------|
| `GraphStore::subgraph(seeds, radius)` | `cih-graph-store/src/lib.rs` | ✅ used for result expansion |
| `GraphStore::communities()` | `cih-graph-store/src/lib.rs` | ✅ for grouping results |
| `Node`, `NodeKind`, `NodeId` | `cih-core/src/lib.rs` | ✅ |
| `GraphArtifacts::read_nodes` | `cih-core` | ✅ JSONL I/O |
| `latest_graph_artifacts()` | `cih-engine/src/main.rs` | ✅ reuse in embed subcommand |
| `query` MCP tool stub | `cih-server/src/main.rs` | ❌ must add |

**`GraphStore` trait does NOT need a `search()` method.** Search is a cross-cutting concern — BM25
over disk artifacts + pgvector over Postgres — that lives in the server layer, not in the graph
traversal port. The `query` tool in `cih-server` orchestrates both directly.

---

## New crate: `cih-search`

**Location:** `crates/cih-search/`

### `Cargo.toml`
```toml
[package]
name = "cih-search"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
cih-core.workspace = true
serde.workspace = true
serde_json.workspace = true
```

### File structure
```
crates/cih-search/src/
  lib.rs       — public re-exports + SearchHit
  bm25.rs      — BM25 index (inverted index, IDF/TF scoring, k1=1.2, b=0.75)
  tokenize.rs  — camelCase split + lowercase normalization
  rrf.rs       — Reciprocal Rank Fusion (RRF_K = 60)
```

### `tokenize.rs`

```rust
pub fn tokenize(text: &str) -> Vec<String>
```

Split on whitespace, `.`, `/`, `#`, `:`, `_`, and camelCase boundaries (insert split before each
uppercase char that follows a lowercase char). Lowercase all tokens. Filter empty tokens and
single-char tokens.

Example: `"OwnerService#findAll/2"` → `["owner", "service", "find", "all"]`

### `bm25.rs`

**Fixed parameters:**
```rust
const K1: f32 = 1.2;
const B: f32  = 0.75;
```

**Index structures:**
```rust
pub struct SearchIndex {
    docs:     Vec<IndexedDoc>,
    inverted: HashMap<String, Vec<(usize, u32)>>,  // term → [(doc_idx, tf)]
    df:       HashMap<String, u32>,                 // term → document frequency
    avgdl:    f32,
    n:        usize,                                // total doc count
}

pub struct IndexedDoc {
    pub node_id:        NodeId,
    pub name:           String,
    pub qualified_name: String,
    pub file:           String,
    pub kind:           NodeKind,
    pub start_line:     u32,
}
```

**Build:**
```rust
pub fn build(nodes: &[Node]) -> SearchIndex
```
For each eligible node, generate document text:
```
"{kind_name} {name} {qualified_name} {file_path}"
```
Tokenize with `tokenize()`. Compute per-term tf (term frequency in this doc). Build inverted index
and df map. Compute `avgdl` (average doc token count across all docs).

**Eligible node kinds** (index only these — skip File, Folder, Community, Process, Other):
`Class`, `Interface`, `Enum`, `Record`, `Annotation`, `Method`, `Constructor`, `Field`, `Route`

**BM25 scoring:**
```
IDF(t) = ln((N - df(t) + 0.5) / (df(t) + 0.5) + 1)

TF(t, d) = tf(t,d) * (K1 + 1) / (tf(t,d) + K1 * (1 - B + B * doc_len(d) / avgdl))

score(d, q) = Σ_t  IDF(t) * TF(t, d)
```

**Search:**
```rust
pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit>
```
Tokenize query. For each query term present in the index: score all posting-list docs. Accumulate
per-doc. Sort descending. Return top `limit` as `Vec<SearchHit>`.

### `rrf.rs`

```rust
pub const RRF_K: usize = 60;

pub struct SearchHit {
    pub node_id:        NodeId,
    pub name:           String,
    pub kind:           NodeKind,
    pub file:           String,
    pub start_line:     u32,
    pub score:          f64,
    pub rank:           usize,
    pub sources:        Vec<String>,         // "bm25" | "semantic"
    pub bm25_score:     Option<f64>,
    pub semantic_score: Option<f64>,
}

pub fn rrf_merge(bm25: &[SearchHit], semantic: &[SearchHit], limit: usize) -> Vec<SearchHit>
```

**RRF algorithm** (port of `hybrid-search.ts::mergeWithRRF`):
```
for bm25 result at index i (0-based):
    rrf_score = 1.0 / (RRF_K + i + 1)    ← rank is 1-indexed in formula
    insert into merged map keyed by node_id, sources = ["bm25"]

for semantic result at index i (0-based):
    rrf_score = 1.0 / (RRF_K + i + 1)
    if node_id in map: add scores, push "semantic" to sources, set semantic_score
    else: create new entry, sources = ["semantic"]

sort merged by score descending → take limit → assign final ranks (1-based)
```

---

## New crate: `cih-embed`

**Location:** `crates/cih-embed/`

### `Cargo.toml`
```toml
[package]
name = "cih-embed"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
cih-core.workspace = true
anyhow.workspace = true
serde.workspace = true
serde_json.workspace = true
blake3.workspace = true
fastembed.workspace = true
tokio-postgres.workspace = true
pgvector = { workspace = true, features = ["postgres"] }
tokio.workspace = true
```

### File structure
```
crates/cih-embed/src/
  lib.rs      — public API: EmbedConfig, EmbedModelKind, EmbedStore, EmbedStats, SemanticHit
  chunker.rs  — character-based chunking (port character-chunk.ts)
  text.rs     — node_document_text(), embedding_text() — what to embed
  store.rs    — pgvector DDL + batch upsert + HNSW index creation
  search.rs   — semantic_search: HNSW vector query + exact-scan fallback
```

### `chunker.rs` — port of `character-chunk.ts`

```rust
pub struct Chunk {
    pub text:       String,
    pub chunk_idx:  u32,
    pub start_line: u32,
    pub end_line:   u32,
}

pub fn character_chunk(
    content: &str,
    start_line: u32,
    end_line: u32,
    chunk_size: usize,   // default 1200
    overlap: usize,      // default 120
) -> Vec<Chunk>
```

Algorithm (port of `character-chunk.ts`):
1. If `content.len() ≤ chunk_size`: return a single chunk.
2. Build a line-offset array: byte position of each `\n` boundary, for line-range resolution.
3. Sliding window with `offset = 0`, `chunk_idx = 0`:
   - `end = (offset + chunk_size).min(content.len())`
   - Extract `&content[offset..end]` as chunk text.
   - Resolve start_line/end_line from the offset array.
   - Create `Chunk`, increment chunk_idx.
   - Advance `offset = end - overlap`.
   - Stop when `end >= content.len()`.

### `text.rs`

```rust
/// Input text for chunking and indexing (no source body available in artifacts).
pub fn node_document_text(node: &Node) -> String {
    format!("{} {} {} {}", node.kind.cypher_label(), node.name,
            node.qualified_name.as_deref().unwrap_or(""), node.file)
}

/// Text to embed for a given chunk (prepends kind + fqcn for model context).
pub fn embedding_text(node: &Node, chunk_text: &str) -> String {
    format!("{}: {}\n{}", node.kind.cypher_label(),
            node.qualified_name.as_deref().unwrap_or(&node.name),
            chunk_text)
}
```

### `store.rs` — pgvector

**Schema (DDL):**
```sql
CREATE TABLE IF NOT EXISTS cih_embeddings (
    node_id      TEXT        NOT NULL,
    chunk_idx    INTEGER     NOT NULL,
    start_line   INTEGER     NOT NULL DEFAULT 0,
    end_line     INTEGER     NOT NULL DEFAULT 0,
    embedding    vector(384) NOT NULL,
    content_hash TEXT        NOT NULL,
    PRIMARY KEY (node_id, chunk_idx)
);
CREATE INDEX IF NOT EXISTS cih_embeddings_hnsw
    ON cih_embeddings USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);
```

**Key functions:**
```rust
pub async fn ensure_schema(client: &tokio_postgres::Client) -> Result<()>

pub async fn batch_upsert(
    client: &tokio_postgres::Client,
    rows: &[EmbeddingRow],
) -> Result<usize>
// EmbeddingRow { node_id: String, chunk_idx: i32, start_line: i32, end_line: i32,
//                embedding: Vec<f32>, content_hash: String }

pub async fn existing_hashes(
    client: &tokio_postgres::Client,
    node_ids: &[&str],
) -> Result<HashMap<String, String>>   // node_id → content_hash
```

Upsert uses `INSERT ... ON CONFLICT (node_id, chunk_idx) DO UPDATE SET ...` for idempotency.

### `lib.rs` — public API

```rust
pub struct EmbedConfig {
    pub pg_url:     String,
    pub model:      EmbedModelKind,
    pub chunk_size: usize,    // 1200
    pub overlap:    usize,    // 120
    pub batch_size: usize,    // 64 (texts per fastembed call)
}
impl Default for EmbedConfig { /* chunk_size=1200, overlap=120, batch_size=64, model=MiniLmL6V2 */ }

pub enum EmbedModelKind {
    MiniLmL6V2,      // AllMiniLML6V2 — 384-dim, ~22M params, good for code names
    BgeSmallEnV15,   // BGESmallENV15  — 384-dim alternative
}

pub struct EmbedStore {
    client: tokio_postgres::Client,
    model:  fastembed::TextEmbedding,
    config: EmbedConfig,
}

pub struct EmbedStats {
    pub nodes_processed: usize,
    pub chunks_generated: usize,
    pub chunks_skipped:  usize,    // unchanged (content_hash match)
    pub chunks_inserted: usize,
}

pub struct SemanticHit {
    pub node_id:    NodeId,
    pub file:       String,
    pub start_line: u32,
    pub end_line:   u32,
    pub distance:   f32,     // cosine distance (0.0 = identical)
}

impl EmbedStore {
    pub async fn connect(config: EmbedConfig) -> Result<Self>;
    pub async fn embed_nodes(&self, nodes: &[Node]) -> Result<EmbedStats>;
    pub async fn semantic_search(&self, query: &str, k: usize, max_distance: f32) -> Result<Vec<SemanticHit>>;
}
```

**`embed_nodes` algorithm:**
1. Filter to embeddable node kinds (Class, Interface, Enum, Record, Annotation, Method,
   Constructor, Field — same list as BM25).
2. For each node: `text = node_document_text(node)`, then `character_chunk(text, ...)`.
3. Per chunk: `content_hash = &blake3::hash(format!("v1\n{}\n{}", node.id.as_str(), chunk.text).as_bytes()).to_hex()[..16]`.
4. Fetch `existing_hashes` for this node batch. Skip chunks whose hash matches (no change).
5. For non-skipped chunks: collect `embedding_text(node, chunk.text)`.
6. Sub-batch embed: `model.embed(texts_batch, None)?` → `Vec<Vec<f32>>`.
7. `batch_upsert` all rows.

**`semantic_search` algorithm:**
1. Embed query: `model.embed(vec![query.to_string()], None)?` → first vector.
2. Run HNSW query:
   ```sql
   SELECT node_id, chunk_idx, start_line, end_line,
          embedding <=> $1 AS distance
   FROM cih_embeddings
   WHERE embedding <=> $1 < $2
   ORDER BY distance
   LIMIT $3
   ```
3. Deduplicate by `node_id`: keep minimum distance per node.
4. Return top-k sorted by distance ascending.
5. **Exact-scan fallback:** If the HNSW query fails (extension missing), fetch all rows and
   compute cosine distance in Rust. Cap at 50 000 rows.

---

## Engine changes: `cih-engine`

### `cih-engine/Cargo.toml`
Add `cih-embed.workspace = true`.

### New `Embed` subcommand

```rust
/// Generate embeddings for the latest analyzed graph and store in pgvector.
Embed {
    repo: PathBuf,
    #[arg(long, env = "CIH_PG_URL",
          default_value = "postgres://postgres:cih@localhost:5432/cih")]
    pg_url: String,
    #[arg(long, default_value = "mini-lm", help = "mini-lm | bge-small")]
    model: String,
    #[arg(long)]
    json: bool,
},
```

### `run_embed(repo, pg_url, model, json)`:
```
1. latest_graph_artifacts(&repo)?   ← reuse existing function
2. nodes = source.read_nodes()?
3. config = EmbedConfig { pg_url, model: parse_model(&model), .. Default::default() }
4. store = EmbedStore::connect(config).await?
5. store.ensure_schema().await?     ← idempotent DDL
6. stats = store.embed_nodes(&nodes).await?
7. print/json summary (nodes_processed, chunks_inserted, chunks_skipped)
```

---

## MCP server changes: `cih-server`

### `cih-server/Cargo.toml`
Add `cih-search.workspace = true` and `cih-embed.workspace = true`.

### `CihServer` struct gains search state:
```rust
pub struct CihServer {
    store:       Arc<dyn GraphStore>,
    bm25:        Arc<tokio::sync::RwLock<Option<SearchIndex>>>,  // lazily built
    embed_store: Option<Arc<EmbedStore>>,
    artifacts_dir: Option<PathBuf>,  // for BM25 index source
}
```

### Server CLI / `connect()` additions:
- Read `CIH_ARTIFACTS_DIR` env var (or `--artifacts-dir` arg) → `artifacts_dir`
- Read `CIH_PG_URL` env var (or `--pg-url` arg) → optionally create `EmbedStore`

### New `query` tool:

```rust
#[derive(Deserialize, JsonSchema)]
struct QueryArgs {
    q: String,
    #[serde(default = "default_query_limit")]  // 10
    limit: usize,
    #[serde(default)]
    expand: bool,   // if true: call subgraph(top-5 seeds, radius=1)
}

#[tool(description = "Hybrid BM25 + semantic search over the codebase. \
    Returns ranked code symbols for a natural-language query, \
    optionally with a 1-hop subgraph around the top results.")]
async fn query(
    &self,
    Parameters(args): Parameters<QueryArgs>,
) -> Result<CallToolResult, McpError>
```

**Implementation:**
1. **BM25:** If `self.artifacts_dir` is set, lazily build `SearchIndex::build(&nodes)` on first
   call (write-lock, check Option, if None load nodes from latest JSONL, build index, store).
   Run `index.search(&args.q, args.limit * 2)`.
2. **Semantic:** If `self.embed_store` is Some, run `embed_store.semantic_search(&args.q,
   args.limit * 2, 0.5)`. Map `SemanticHit` → `SearchHit` with `sources = vec!["semantic"]`.
3. **Merge:** `rrf_merge(&bm25_hits, &semantic_hits, args.limit)`.
4. **Expand:** If `args.expand`, collect top-5 NodeIds → `self.store.subgraph(&seeds, 1).await?`.
5. Return `QueryResult { hits: merged, subgraph: Option<Subgraph> }` serialized to JSON.

If both BM25 and semantic are unavailable (no artifacts_dir, no pg_url): return an error with
a clear message explaining which env vars to set.

---

## Workspace `Cargo.toml` changes

Add to `[workspace.members]`:
```toml
"crates/cih-search",
"crates/cih-embed",
```

Add to `[workspace.dependencies]`:
```toml
fastembed   = { version = "4", default-features = false, features = ["ort-download-binaries"] }
tokio-postgres = "0.7"
pgvector    = "0.4"
```

---

## Tests (8 required)

### `cih-search` unit tests

1. **`bm25_scores_exact_name_match_highest`** — index 3 nodes (`OwnerService`, `UserService`,
   `RouteController`); `search("owner service", 5)` → `OwnerService` node has the highest score.

2. **`bm25_tokenizer_splits_camel_case`** — `tokenize("OwnerService#findAll/2")` includes
   `"owner"`, `"service"`, `"find"`, `"all"` in the result.

3. **`bm25_empty_corpus_returns_empty`** — `SearchIndex::build(&[]).search("anything", 5)` →
   empty vec, no panic.

4. **`rrf_rank_1_score_is_1_over_61`** — single BM25 result at index 0 (rank 1) →
   `score == 1.0 / 61.0` (i.e., `1 / (RRF_K + 1)`).

5. **`rrf_item_in_both_lists_scores_higher`** — node A appears at rank 1 in BM25 AND rank 1 in
   semantic; node B appears only at rank 1 in BM25. After `rrf_merge`, node A score > node B score.

### `cih-embed` unit tests

6. **`chunker_single_chunk_for_short_content`** — 50-char content, chunk_size=1200, overlap=120
   → exactly 1 chunk whose text equals the input.

7. **`chunker_splits_with_correct_overlap`** — 2500-char content (all `'a'`), chunk_size=1200,
   overlap=120 → at least 2 chunks; the second chunk's first 120 bytes overlap with the end of
   the first chunk (i.e., `chunks[1].text[..120] == chunks[0].text[1080..]`).

8. **`content_hash_is_stable_and_changes_on_mutation`** — same node + text → same hash on two
   calls; change the text by one character → different hash.

---

## Sequencing (implement in this order)

1. **`cih-search`** — pure Rust, no external services. Implement `tokenize.rs` → `bm25.rs` →
   `rrf.rs` → `lib.rs`. Add tests 1–5. `cargo test -p cih-search` must be green.

2. **`cih-embed` chunker + text** (`chunker.rs`, `text.rs`) — no DB or model needed. Add tests 6–8.
   `cargo test -p cih-embed` must be green for these pure tests.

3. **`cih-embed` store + search** (`store.rs`, `search.rs`, `lib.rs`) — requires pgvector. Mark
   any test that needs a real Postgres connection `#[ignore]` with the comment:
   `// requires: docker run -p 5432:5432 -e POSTGRES_PASSWORD=cih pgvector/pgvector:pg16`

4. **`cih-engine` Embed subcommand** — wire `EmbedStore` into the CLI.

5. **`cih-server` query tool** — add lazy BM25 loading, semantic wiring, `query` tool, and the
   `CIH_ARTIFACTS_DIR` / `CIH_PG_URL` env vars.

6. **`ROADMAP.md`** — mark Phase 6 ✅ with verified date and test count.

---

## Verification (end-to-end)

```bash
# 1. Start pgvector
docker run -d --name pgvec -e POSTGRES_PASSWORD=cih \
  -p 5432:5432 pgvector/pgvector:pg16

# 2. Analyze a Java repo (Phase 4 path)
cargo run -p cih-engine -- analyze <java-repo> --all --no-load

# 3. Generate and store embeddings
CIH_PG_URL=postgres://postgres:cih@localhost:5432/cih \
  cargo run -p cih-engine -- embed <java-repo>

# 4. Start MCP server
FALKOR_URL=redis://127.0.0.1:6380 \
CIH_ARTIFACTS_DIR=<java-repo>/.cih/artifacts \
CIH_PG_URL=postgres://postgres:cih@localhost:5432/cih \
  cargo run -p cih-server

# 5. Test via MCP Inspector
# → query("user registration") → ranked hits
# → query("save owner", expand: true) → hits + 1-hop subgraph
# → query("spring controller route") → Route/Method nodes near top

# 6. All tests green
cargo test --workspace
cargo clippy --workspace
```

---

## Critical files summary

| Action | File |
|--------|------|
| **Create** | `crates/cih-search/Cargo.toml` |
| **Create** | `crates/cih-search/src/lib.rs` |
| **Create** | `crates/cih-search/src/bm25.rs` |
| **Create** | `crates/cih-search/src/tokenize.rs` |
| **Create** | `crates/cih-search/src/rrf.rs` |
| **Create** | `crates/cih-embed/Cargo.toml` |
| **Create** | `crates/cih-embed/src/lib.rs` |
| **Create** | `crates/cih-embed/src/chunker.rs` |
| **Create** | `crates/cih-embed/src/text.rs` |
| **Create** | `crates/cih-embed/src/store.rs` |
| **Create** | `crates/cih-embed/src/search.rs` |
| **Edit** | `Cargo.toml` — add 2 members + `fastembed`, `tokio-postgres`, `pgvector` to workspace deps |
| **Edit** | `crates/cih-engine/Cargo.toml` — add `cih-embed.workspace = true` |
| **Edit** | `crates/cih-engine/src/main.rs` — add `Embed` subcommand + `run_embed` |
| **Edit** | `crates/cih-server/Cargo.toml` — add `cih-search`, `cih-embed` |
| **Edit** | `crates/cih-server/src/main.rs` — add `query` tool + lazy BM25 + embed wiring |
| **Edit** | `ROADMAP.md` — mark Phase 6 ✅ when done |

---

## Risks / decisions

- **fastembed first-run download:** Downloads the model (~90 MB) from HuggingFace on first
  `EmbedStore::connect`. Cache lands in `~/.cache/huggingface/`. Any test calling `connect()`
  must be `#[ignore]`d in CI.
- **BM25 without source bodies:** The JSONL artifacts do not contain raw source code (only names,
  types, file paths). BM25 quality is therefore driven by tokenized identifiers — sufficient for
  code symbol search (query "user registration" matches `UserRegistrationService`) but not for
  doc-string search. Full source indexing is a Phase 9+ refinement.
- **BM25 index rebuilt in-memory per server start:** No persistence. For 10k nodes the build takes
  < 500 ms. Incremental updates (Phase 9) will add a persistent index.
- **Semantic search graceful degradation:** If `CIH_PG_URL` is unset, `query` falls back to
  BM25-only. `expand` without a graph store logs a warning and skips the subgraph.
- **384-dim MiniLM vs 1024-dim bge-m3:** Use `AllMiniLML6V2` (MiniLM-384) for Phase 6. It is
  faster, smaller, and has no quality gap for identifier-level code search. bge-m3-1024 is deferred
  to Phase 10+.
- **No AST chunker:** Character chunking is the only chunker in Phase 6. For code nodes with no
  source bodies (just names), character chunking over `node_document_text` produces single short
  chunks in most cases. AST chunking (port of `chunker.ts`) is deferred to Phase 9.
