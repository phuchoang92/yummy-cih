# Scoping: native FalkorDB bulk loader (`GRAPH.BULK`)

## Why

Loading a large graph is the slow step of `analyze`. Measured baseline (fineract,
87 280 nodes / 253 144 edge rows) into FalkorDB v4.18.10: **~33 s** total, split
~15 s node-write + ~16 s edge-write. Per-phase measurement (2026-07-14) proved the
cost is FalkorDB's **intrinsic Cypher-`UNWIND` insert throughput** (~6 k nodes/s,
~15 k edges/s) — *not* something a Cypher-level tweak can move:

| Lever tried | Result |
|---|---|
| `MERGE` → `CREATE` (fresh graph, in-memory dedup) | byte-identical graph, **no speedup** |
| `CREATE` + deferred id-index (build after nodes) | index build **0 ms**, **no speedup** |
| `BATCH` 4000 → 16000 | edges **2.3× slower** (4000 already tuned) |
| client-side sparse props | no-op — FalkorDB already drops NULLs |

The one real lever is FalkorDB's **native bulk-insert protocol** (`GRAPH.BULK`),
which bypasses the Cypher parser/planner and the per-edge `MATCH` entirely.
Reference implementations: the `redisgraph-bulk-loader` (Python) and FalkorDB
`src/bulk_insert/bulk_insert.c` for **v4.18** (format is version-sensitive — pin
to the running module version, currently `ver=41810`). Expected outcome: fineract
load ~33 s → low single-digit seconds (typical 10–50× for cold loads).

## The protocol (what we must emit)

`GRAPH.BULK <key> <op> <node_count> <edge_count> <label_count> <reltype_count>
<label_blobs…> <reltype_blobs…>` — payload chunked into multiple commands
(first carries `BEGIN`, rest continue) to stay under the query-size limit.

Key properties that shape the design:

1. **Implicit node ordinals.** Nodes get a 0-based internal id by insertion order
   across the whole session; relations reference endpoints by that **ordinal**
   (binary `u64`), *not* by our string `id`. So we must assign ordinals and build
   a `HashMap<&str id, u64 ordinal>`, then remap every edge's `src`/`dst`.
2. **Blobs = fixed schema.** One **label blob** per node label; one **reltype
   blob** per relationship type. Each blob = header (name + property-count +
   property-name list, NUL-terminated) followed by packed rows.
3. **Type-tagged values.** Each property value = 1 type byte
   (NULL / BOOL / DOUBLE / LONG / STRING / ARRAY) + payload (LE numerics,
   NUL-terminated strings). A node row = property values only; a relation row =
   `src_ord` (u64 LE) + `dst_ord` (u64 LE) + property values.
4. **No `MATCH`, no index needed** during load — that is the whole win. Indexes
   are created *after* the bulk insert.

## Mapping to our data

- **One label blob, `Symbol`.** Fixed superset schema = the 20 columns
  `load_nodes_edges` writes today (`id, name, kind, file, qualifiedName,
  startLine, endLine, props, stereotype, httpMethod, path, decorator, handler,
  symbolCount, cohesion, processType, cyclomatic, cognitive, loopDepth,
  transitiveLoopDepth`). Encode `NULL` for absent props — FalkorDB skips storing
  them (verified: Function nodes keep 9 keys). `id` stays a normal stored
  property (indexed) *and* seeds the ordinal map.
- **One reltype blob per `EdgeKind`** (CALLS, HANDLES_ROUTE, …), schema
  `confidence, reason, callSites` (callSites = JSON string, as today).
- **Correctness parity with the Cypher path is mandatory** — reproduce exactly:
  - **Dedup nodes by `id`** (first wins) before assigning ordinals — artifacts
    contain 10 duplicate ids; `MERGE` collapses them today.
  - **Dedup edges by `(src, dst, kind)`** (first wins) — 17 duplicate rows today.
  - **Drop dangling edges** whose `src`/`dst` id is not in the ordinal map — the
    current `MATCH (a),(b)` silently drops ~859 such rows. Filtering on the map
    reproduces this (baseline graph = **87 270 nodes / 252 268 edges**).
- **Indexes after load:** `CREATE INDEX FOR (n:Symbol) ON (n.id)` and `ON (n.kind)`
  once the bulk insert completes (measured ~0 ms). Bulk into an **empty, index-free**
  staging graph — so drop indexes / don't `ensure_schema` before the bulk step.

## Code changes

- **New module `crates/cih-falkor/src/bulk.rs`** — the binary encoder: value
  type-tagging, label/reltype blob builders, ordinal assignment + edge remap,
  dedup + dangling filter, and payload chunking. Pure functions over
  `&[Node]`/`&[Edge]` → `Vec<u8>` batches (unit-testable without a live DB).
- **`crates/cih-falkor/src/lib.rs`** — add `FalkorStore::bulk_insert(nodes, edges)`
  that issues the `GRAPH.BULK` command(s) via `redis::cmd` (binary args) and then
  creates the indexes. Route `bulk_load` (fresh staging graph) through it; **keep
  the Cypher `MERGE` path for `upsert_incremental`** (bulk can only build a fresh
  graph) and as the portable fallback for the future Neptune adapter (which has no
  `GRAPH.BULK` — see the module-header go-live note).
- **Multi-set loading** (`crates/cih-engine/src/db.rs::load_many_to_falkor`, which
  loops `bulk_load` over analyze-then-community sets): the community set is tiny
  (a few hundred `Community:N` nodes + membership edges). **First cut: bulk-load
  the big analyze set, load the small community set via the existing Cypher path.**
  This sidesteps cross-set ordinal bookkeeping while capturing ~all the speedup.
  (A later refinement can fold both into one bulk session with a shared ordinal map.)

## Risks

- **Format drift** — the byte layout is FalkorDB-version-specific. Pin to the
  reference for `ver=41810`; guard with the round-trip correctness check below so
  a mismatch fails loudly, not silently.
- **Encoding bugs** (endianness, type tags, string termination) corrupt the graph
  quietly. Mitigate with the byte-identical verification and per-type unit tests.
- **`props` JSON column** must encode identically to the Cypher path (a STRING).

## Verification

- **Byte-identical graph vs the Cypher path (gate):** after loading fineract via
  `GRAPH.BULK`, assert `count(n) == 87 270`, `count(r) == 252 268`, per-`kind`
  edge counts equal `docs`/the saved baseline, and spot-check node properties
  (`keys(n)`, `props`, complexity fields) + a `callSites` edge match the Cypher
  load. Re-run `trace_flow` on nestprisma for an end-to-end smoke.
- **Speed:** compare total `bulk_load` wall-time before/after on fineract (and a
  ~600 k-node repo if available). Ship only on a real, repeatable win.
- **Unit tests** in `bulk.rs`: value encoding per type, ordinal remap, dedup +
  dangling drop, chunk boundaries.
- **Gates:** `cargo test --workspace`, `clippy -D warnings`, `fmt --check`. No
  `PARSE_CACHE_SCHEMA` bump (load-only; parser output untouched).

## Effort

~1–2 days: the encoder + `GRAPH.BULK` wiring is the bulk of it; dedup/ordinal/
dangling logic is small and already specified by the Cypher path's observed
behavior. Medium risk, fully gated by the byte-identical correctness check.
