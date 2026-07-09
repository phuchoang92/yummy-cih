# Runbook: Exercising `--feature-strategy embed` against local Postgres

Operational steps to run and verify the embedding-based feature clusterer end-to-end on a
developer machine. Design/rationale live in [`../plans/embed-feature-clustering.md`](../plans/embed-feature-clustering.md);
this doc is the "how do I actually run it" companion.

The path is: **`analyze` → `embed` → `discover --feature-strategy embed`**. `embed` populates
pgvector (per-chunk vectors *and* the per-node `cih_node_vectors` table); `discover` reads those
vectors, builds a k-NN graph, runs Leiden, and writes `groups.jsonl`.

---

## 0. Prerequisites

- Rust stable, Docker.
- A built engine: `cargo build -p cih-engine` (debug binary at `./target/debug/cih-engine`).
- A target Java/Spring repo already analyzable by CIH. Examples below use `$REPO`; a repo under
  `cih-eval-repos/` (e.g. `cih-eval-repos/fineract`) works well.

```bash
cd /Users/phuc/BigMoves/AI/yummy-cih
export REPO=cih-eval-repos/fineract          # <-- your target repo
export ENGINE=./target/debug/cih-engine
```

---

## 1. Start Postgres (pgvector) on 5433

The compose file ships `pgvector/pgvector:pg16` mapped to host port **5433** (5432 is left for a
local/Homebrew Postgres). The password comes from `.env` (`POSTGRES_PASSWORD`).

```bash
# .env already sets POSTGRES_USER=cih, POSTGRES_DB=cih, POSTGRES_PASSWORD=...
# Start only Postgres (FalkorDB not needed for the embed path itself; add `falkordb`
# if you also want `discover` to load into the graph DB).
docker compose up -d postgres

# Wait until it's accepting connections
until docker compose exec -T postgres pg_isready -U cih >/dev/null 2>&1; do sleep 1; done
echo "postgres ready"
```

Point the engine at it. Read the password from `.env` so the URL always matches the container:

```bash
export POSTGRES_PASSWORD=$(grep -E '^POSTGRES_PASSWORD=' .env | cut -d= -f2)
export CIH_PG_URL="postgres://cih:${POSTGRES_PASSWORD}@localhost:5433/cih"
psql "$CIH_PG_URL" -c '\conninfo'      # optional sanity check (needs psql installed)
```

> The `pgvector/pgvector:pg16` image supplies the `vector` extension, HNSW indexes, and the
> `avg(vector)` aggregate the per-node refresh relies on. Any Postgres ≥ pgvector 0.5 works.

---

## 2. Analyze, then embed

```bash
# 2a. Analyze — writes .cih/artifacts/<version>/ (nodes.jsonl, edges.jsonl)
FALKOR_URL=redis://localhost:6380 CIH_GRAPH_KEY=cih \
  "$ENGINE" analyze "$REPO" --all

# 2b. Embed — writes per-chunk vectors AND refreshes cih_node_vectors + its HNSW index.
#     --pg-url is optional here since CIH_PG_URL is exported.
"$ENGINE" embed "$REPO" --pg-url "$CIH_PG_URL"
```

Expected `embed` output: a summary line with `Nodes: N read, M embeddable` and
`Chunks: … embedded`. On a re-run with no code changes, `embedded` drops to ~0 and
`skipped unchanged` rises (BLAKE3 content-hash short-circuit).

**Confirm the per-node table got populated** (this is the new artifact the clusterer needs):

```bash
psql "$CIH_PG_URL" -c "SELECT count(*) AS node_vectors,
                              (SELECT count(*) FROM cih_embeddings) AS chunk_rows
                       FROM cih_node_vectors;"

# HNSW index should exist at node granularity:
psql "$CIH_PG_URL" -c "\di cih_node_vectors_hnsw_idx"
```

`node_vectors` should be > 0 and roughly the embeddable-node count. If it's 0, the discover step
still self-heals (it backfills from `cih_embeddings`), but seeing it here confirms the embed-time
refresh path fired.

---

## 3. Discover with the embed strategy

```bash
"$ENGINE" discover "$REPO" \
  --feature-strategy embed \
  --pg-url "$CIH_PG_URL" \
  --no-load                     # skip FalkorDB load; drop this to also load the graph DB
```

Watch the log line `embed clustering: Leiden produced clusters` — it reports `clusters=` and
`assigned=`. The feature artifact is written to:

```
$REPO/.cih/artifacts-features/<source-version>/groups.jsonl
```

---

## 4. Inspect the output

```bash
FEATDIR=$(ls -d "$REPO"/.cih/artifacts-features/*/ | tail -1)
echo "$FEATDIR"

# All entries are strategy="embed"
jq -r '.strategy' "$FEATDIR/groups.jsonl" | sort | uniq -c

# Top feature slugs by node count — expect meaningful names, not all "cluster-N"
jq -r '.name' "$FEATDIR/groups.jsonl" | sort | uniq -c | sort -rn | head -20

# Fraction landing in "shared" (unclustered/unembedded) — want this well under 50%
total=$(wc -l < "$FEATDIR/groups.jsonl")
shared=$(jq -r 'select(.name=="shared") | .node_id' "$FEATDIR/groups.jsonl" | wc -l)
echo "shared: $shared / $total"

# Spot-check evidence + confidence on a few clustered entries
jq -c 'select(.name!="shared") | {name, confidence, evidence}' "$FEATDIR/groups.jsonl" | head
```

**What "good" looks like:** most nodes assigned to slugs (not `shared`), slugs are readable
(`payments`, `banking-overdraft`, …) rather than mostly `cluster-0/1/2`, and evidence reads
`knn-leiden k=15 thr=0.65 res=0.80 sim=0.xxx`.

Compare against the package strategy to confirm embed merges cross-package domain nodes:

```bash
"$ENGINE" discover "$REPO" --feature-strategy package --no-load
# then diff the feature names / membership between the two artifact dirs
```

---

## 5. Verification checks (maps to the plan's step 5–7)

### Determinism (near-, not bit-exact)
Run discover twice and compare membership. ANN recall + float summation perturb ties, so assert
**high stability**, not identity:

```bash
run() { "$ENGINE" discover "$REPO" --feature-strategy embed --pg-url "$CIH_PG_URL" --no-load >/dev/null;
        d=$(ls -d "$REPO"/.cih/artifacts-features/*/ | tail -1);
        jq -r '[.node_id,.name] | @tsv' "$d/groups.jsonl" | sort; }
run > /tmp/embed_run1.tsv
run > /tmp/embed_run2.tsv
echo "identical lines: $(comm -12 /tmp/embed_run1.tsv /tmp/embed_run2.tsv | wc -l) / $(wc -l < /tmp/embed_run1.tsv)"
# Slug *names* can shift while membership holds; for a membership-only check, compare column 1↔cluster grouping.
```

### Degradation — no Postgres
Should warn and fall back to package cleanly, never panic:

```bash
env -u CIH_PG_URL "$ENGINE" discover "$REPO" --feature-strategy embed --no-load
# Expect log: "embed feature strategy unavailable — falling back to package
#              (did you run `cih embed --pg-url` first?)"
```

### Degradation — threshold too high (no edges)
```bash
"$ENGINE" discover "$REPO" --feature-strategy embed --pg-url "$CIH_PG_URL" \
  --embed-similarity-threshold 0.99 --no-load
# Expect: "no k-NN edges above similarity threshold …", everything → shared.
```

### Tuning knobs
```bash
# Finer clusters: raise resolution and/or k
"$ENGINE" discover "$REPO" --feature-strategy embed --pg-url "$CIH_PG_URL" \
  --embed-leiden-resolution 1.2 --embed-knn 25 --embed-similarity-threshold 0.6 --no-load
```

These are also persistable in `cih.toml` (`[discover] embed_similarity_threshold`, `embed_knn`,
`embed_leiden_resolution`) and appear in `cih-engine config show`.

---

## 6. Direct SQL sanity checks (optional)

```bash
# Per-node vector dimension is 384 and rows carry metadata:
psql "$CIH_PG_URL" -c "SELECT node_kind, count(*) FROM cih_node_vectors GROUP BY 1 ORDER BY 2 DESC;"

# Spot-check the k-NN query the engine runs (top-5 neighbors of one node):
psql "$CIH_PG_URL" <<'SQL'
WITH q AS (SELECT node_id, embedding FROM cih_node_vectors LIMIT 1)
SELECT q.node_id AS src, n.node_id AS dst,
       round((1.0 - (q.embedding <=> n.embedding))::numeric, 3) AS sim
FROM q CROSS JOIN LATERAL (
  SELECT node_id, embedding FROM cih_node_vectors n
  WHERE n.node_id <> q.node_id
  ORDER BY n.embedding <=> q.embedding, n.node_id LIMIT 5
) n;
SQL
```

---

## 7. Incremental re-run

Edit a few files in `$REPO`, then:

```bash
FALKOR_URL=redis://localhost:6380 CIH_GRAPH_KEY=cih "$ENGINE" analyze "$REPO" --all
"$ENGINE" embed "$REPO" --pg-url "$CIH_PG_URL"        # re-embeds only changed chunks (seconds)
"$ENGINE" discover "$REPO" --feature-strategy embed --pg-url "$CIH_PG_URL" --no-load
```

The `cih_node_vectors` table is re-aggregated for changed nodes and pruned to the current node
set every embed run, so deleted/renamed classes don't leak into clusters.

---

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Falls back to package immediately | `CIH_PG_URL` unset/wrong, or Postgres down. Check `psql "$CIH_PG_URL" -c '\conninfo'`. |
| `no embeddings found for current nodes` | You skipped `cih embed`, or embedded a different repo/version. Re-run step 2b. |
| `cih_node_vectors` count is 0 after embed | Old binary or the refresh didn't run; discover will self-heal (backfill), but rebuild the engine to get the embed-time refresh. |
| Everything lands in `shared` | Threshold too high (lower `--embed-similarity-threshold`) or embeddings missing for most nodes. |
| Slugs are mostly `cluster-N` | Label nodes have generic names/paths; not a failure, but try more descriptive module dirs or accept the numeric fallback. |
| `function avg(vector) does not exist` | pgvector too old; use `pgvector/pgvector:pg16` (compose default). |
| Port 5433 refused | `docker compose up -d postgres` not run, or a different service holds 5433. |
