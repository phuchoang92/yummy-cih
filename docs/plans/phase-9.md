# Phase 9 — Incremental Re-index (detailed plan)

When a developer updates their branch and re-runs `cih-engine analyze`, the engine currently
re-parses the entire codebase from scratch — even if only 2 files changed out of 12,000.
This makes re-indexing slow on large Spring repos and blocks the "always-fresh graph" workflow.

Phase 9 solves this: detect which files changed since the last run, re-parse only those plus
their transitive importers (because cross-file resolution depends on them), and reuse cached
parse results for everything else. Full resolution still runs on the complete file set.

---

## What Already Exists (do not re-implement)

| Item | Location | Status |
|------|----------|--------|
| `analyze_from_scope()` DB-free core | `cih-engine/src/analyze.rs:149` | ✅ entry point to extend |
| `cih_parse::parse_files(root, files)` | `cih-parse/src/lib.rs` | ✅ already accepts any file slice |
| `ParsedFile` with `Serialize`/`Deserialize` | `cih-core/src/lib.rs` | ✅ can be JSON-cached |
| `content_version()` blake3 hash | `cih-engine/src/versioning.rs:7` | ✅ unchanged |
| `prune_other_versions()` | `cih-engine/src/versioning.rs:102` | ✅ unchanged |
| `rayon` parallel iter | `cih-engine` | ✅ use for parallel file hashing |
| `blake3::Hasher` | `cih-engine/src/versioning.rs` | ✅ same pattern for per-file hashing |
| `swap_version()` | `cih-falkor/src/lib.rs:156` | ⚠️ writes meta node only — replace with blue-green |

**Key invariant to preserve:** `cih_resolve::resolve_edges` is cross-file — a change in
`Foo.java` may affect how `Bar.java` resolves its call targets. Only the **parse step** is
incremented; **resolution always runs over the full file set**.

---

## Three-Part Implementation

### Part 1 — File-Hash Index

**New file: `crates/cih-engine/src/file_cache.rs`**

Persisted at `.cih/file-hashes.json` alongside `scope.json`. Maps repo-relative path →
blake3 content hash (first 16 hex chars, matching `content_version()` style).

```rust
pub(crate) struct FileHashIndex(HashMap<String, String>);

impl FileHashIndex {
    pub(crate) fn load(cih_dir: &Path) -> Self         // empty index if file absent
    pub(crate) fn save(&self, cih_dir: &Path) -> Result<()>
    /// Returns keys in `current` whose value differs from `self` (new or changed files).
    pub(crate) fn changed_files<'a>(&self, current: &'a HashMap<String, String>) -> Vec<&'a str>
}

/// blake3, first 16 hex chars. Reads file from disk.
pub(crate) fn hash_file(repo_root: &Path, rel: &str) -> Result<String>

/// Hash all scope files in parallel (rayon). Returns rel → hash map.
pub(crate) fn hash_all(repo_root: &Path, files: &[String]) -> HashMap<String, String>
```

Updated at the end of every successful `analyze_from_scope` run.

---

### Part 2 — Per-File Parse Cache

**In `crates/cih-engine/src/file_cache.rs`** (continued)

Cache dir: `.cih/parse-cache/<content-hash>.json`. One file per `ParsedFile`, named by the
file's content hash. A file whose content didn't change has the same hash → cached entry is
loaded instead of re-invoking tree-sitter.

```rust
pub(crate) fn load_cached_parsed(cih_dir: &Path, file_hash: &str) -> Option<ParsedFile>
pub(crate) fn save_cached_parsed(cih_dir: &Path, file_hash: &str, parsed: &ParsedFile) -> Result<()>
```

Cache is append-only within a run. A future `cih-engine cleanup --parse-cache` can prune
unreferenced entries (stretch goal, not in Phase 9).

---

### Part 3 — Importer-BFS Expansion

**In `crates/cih-engine/src/file_cache.rs`** (or split to `importer_index.rs` if it grows)

When file X changes, files that import X may also need re-parsing (their resolution depends
on X's type definitions). BFS expands the changed set up to depth 4.

```rust
/// Reverse import map: imported short-name/FQCN-prefix → files that import it.
/// Built in-memory from ParsedFile.imports of ALL cached files in scope.
pub(crate) struct ImporterIndex(HashMap<String, Vec<String>>);

impl ImporterIndex {
    pub(crate) fn build(parsed_files: &[ParsedFile]) -> Self
    /// BFS from `changed` set, expanding transitive importers up to `depth` hops.
    pub(crate) fn expand(&self, changed: &[String], depth: usize) -> HashSet<String>
}
```

Default depth: 4.

---

## Modified `analyze_from_scope` Flow

**Edit `crates/cih-engine/src/analyze.rs`**

Replace the single `cih_parse::parse_files(&root, &scope_file.files)` call (line 156) with:

```
1.  Load FileHashIndex from .cih/file-hashes.json → prev_hashes
2.  hash_all() all scope files in parallel → curr_hashes
3.  changed_files = prev_hashes.changed_files(&curr_hashes)
4.  If changed_files.empty() AND prev_hashes covers all scope files:
      → log "nothing changed, reusing last artifacts"
        return early using latest_graph_artifacts() (no parse, no resolve, no DB reload)
5.  Load cached ParsedFiles for unchanged files from .cih/parse-cache/
6.  Build ImporterIndex from all cached ParsedFiles
7.  expanded_changed = importer_index.expand(&changed_files, depth=4)
8.  Re-parse only files in expanded_changed (via cih_parse::parse_files)
9.  save_cached_parsed() for each newly parsed file
10. Combine re-parsed + cached → all_parsed_files  (same Vec<ParsedFile> shape as before)
11. Continue identically from here:
      resolve_edges, extract_jar_api, content_version, write artifacts,
      prune_other_versions, load to FalkorDB, swap_version
12. save FileHashIndex (update prev_hashes with curr_hashes)
```

**New flag on `Analyze` subcommand in `cih-engine/src/main.rs`:**
```
--no-cache   Disable incremental mode; re-parse all files (same as pre-Phase-9 behavior).
             Use when the parser itself changes (e.g., after a tree-sitter upgrade).
```

---

## Atomic Version Swap (Blue-Green)

Currently `swap_version()` in `cih-falkor/src/lib.rs:156` only writes a `_CihMeta` node —
the live graph receives incremental writes mid-load, creating a brief inconsistent window.

**Replace with a staging-key + `GRAPH.COPY` approach:**

### `cih-graph-store/src/lib.rs`

Add to `GraphStore` trait:
```rust
/// Copy this store's graph into `dest_key`, replacing it atomically (GRAPH.COPY … REPLACE).
/// Used for blue-green publish: bulk-load into a staging key, then publish to the live key.
async fn publish_to(&self, dest_key: &str) -> Result<()>;
```

### `cih-falkor/src/lib.rs`

```rust
async fn publish_to(&self, dest_key: &str) -> Result<()> {
    // FalkorDB: GRAPH.COPY <src> <dst> REPLACE  — atomic, dst is replaced in one command
    self.run(&format!("GRAPH.COPY {} {} REPLACE", self.graph_key, dest_key)).await?;
    Ok(())
}
```

Also add `drop_graph()` for staging cleanup:
```rust
pub async fn drop_graph(&self) -> Result<()> {
    self.run(&format!("GRAPH.DELETE {}", self.graph_key)).await?;
    Ok(())
}
```

### `cih-engine/src/db.rs`

Replace `load_to_falkor()` with blue-green flow:

```
1. Connect to FalkorStore with key = "<graph_key>-staging"
2. ensure_schema() on staging key
3. bulk_load(artifacts) into staging
4. staging_store.publish_to(graph_key)      // GRAPH.COPY staging → live (atomic)
5. staging_store.drop_graph()               // GRAPH.DELETE staging (cleanup)
```

Live graph is never in a mid-load state. The old `swap_version()` meta-node write is removed.

---

## Files to Create / Edit

| Action | File | Change |
|--------|------|--------|
| **New**  | `crates/cih-engine/src/file_cache.rs` | `FileHashIndex`, `ImporterIndex`, `hash_all`, `hash_file`, parse cache I/O |
| **Edit** | `crates/cih-engine/src/analyze.rs` | Integrate incremental flow in `analyze_from_scope`; early-return on no-op |
| **Edit** | `crates/cih-engine/src/main.rs` | Add `--no-cache` flag to `Analyze` subcommand; register `file_cache` module |
| **Edit** | `crates/cih-graph-store/src/lib.rs` | Add `publish_to()` to `GraphStore` trait |
| **Edit** | `crates/cih-falkor/src/lib.rs` | Implement `publish_to()` + `drop_graph()`; remove old `swap_version` meta-node logic |
| **Edit** | `crates/cih-engine/src/db.rs` | Blue-green load: staging key → `bulk_load` → `publish_to` → `drop_graph` |

No new crates. No `NodeKind`/`EdgeKind` changes. `ParsedFile` is already serializable.

---

## Tests (≈ 7 new)

Add to `crates/cih-engine/src/file_cache.rs` (`#[cfg(test)]`) and `crates/cih-engine/src/tests.rs`:

1. **`file_hash_index_round_trips`** — write + reload `FileHashIndex`; entries match
2. **`changed_files_detects_addition_and_modification`** — 3 files, change 1 → only 1 in changed set
3. **`parse_cache_round_trips`** — cache a `ParsedFile` by hash, load it back; fields match
4. **`importer_index_bfs_depth_1`** — file A imports B; B changes → expanded = {A, B}
5. **`importer_index_bfs_respects_depth`** — chain A→B→C→D; depth 2 → {B, C}, not D
6. **`incremental_noop_when_files_unchanged`** — second analyze run with same files → same version, no re-parse
7. **`incremental_bumps_version_on_single_file_change`** — edit 1 file → version hash changes

---

## Verification

```bash
# 1. Cold start — full index
cih-engine analyze --all --repo /path/to/spring-repo
# Expect: all files parsed, artifacts written, FalkorDB loaded via blue-green staging

# 2. Edit one file
echo "// changed" >> /path/to/spring-repo/src/main/java/com/example/Foo.java

# 3. Incremental re-index
cih-engine analyze --all --repo /path/to/spring-repo
# Expect: only Foo.java + its importers parsed (~5-20 files); resolve runs full; FalkorDB reloaded

# 4. No-op re-index (nothing changed)
cih-engine analyze --all --repo /path/to/spring-repo
# Expect: log "nothing changed, reusing last artifacts" — no parse, no DB reload

# 5. Force full re-index
cih-engine analyze --all --no-cache --repo /path/to/spring-repo
# Expect: all files parsed (pre-Phase-9 behavior)

# 6. All tests green
cargo test --workspace
cargo clippy --workspace
```

---

## Expected Impact

| Scenario | Before Phase 9 | After Phase 9 |
|----------|---------------|---------------|
| Edit 1 file in 12k-file repo | Parse 12k files (~90s) | Parse ~10 files (<5s) |
| No changes | Parse 12k files (~90s) | 0 files parsed, no DB reload (<1s) |
| Parser upgraded (--no-cache) | Same as before | Same as before |
| FalkorDB reload | In-place, brief inconsistent window | Blue-green, always consistent |

---

## Sequencing

1. **`file_cache.rs`** — `FileHashIndex`, `hash_all`, parse cache functions, `ImporterIndex`.
   Unit-test with `cargo test -p cih-engine`.
2. **`analyze.rs`** — integrate incremental flow + `--no-cache` flag.
   `cargo test -p cih-engine` (integration tests in `tests.rs`).
3. **`cih-graph-store` + `cih-falkor`** — `publish_to`, `drop_graph`, blue-green `db.rs`.
   `cargo test --workspace`.
4. **ROADMAP.md** — mark Phase 9 ✅ with test count.
