# Plan: Discover Load Preservation

## Bug

`cih-engine discover` replaces the live FalkorDB graph with community-only data.

After `analyze`, FalkorDB holds the full code graph (classes, methods, fields, edges).
After `discover`, line 27 of `discover.rs` calls:

```rust
load_to_falkor(url, key, &emit.artifacts)
```

`emit.artifacts` is the `GraphArtifacts` written to `.cih/artifacts-community/` — containing
only `Community` and `Process` nodes. `load_to_falkor` drops the staging graph, loads those
nodes only, then publishes, overwriting the live graph. The full code graph is gone.

**Intended state after `discover`:** FalkorDB holds analyzed code nodes + community/process
enrichment, so MCP tools can traverse from a community into its member methods and classes.

---

## What does NOT change

- Disk layout: analyze artifacts stay under `.cih/artifacts/<version>`, discover artifacts
  stay under `.cih/artifacts-community/<version>`.
- `analyze`, `resolve`: still call `load_to_falkor` unchanged; no callers outside `discover`
  are affected.
- `discover --no-load`: disk-only path is entirely unaffected.
- `DiscoverSummary` JSON key names: `falkor_nodes` and `falkor_edges` keep their names;
  they will report summed counts after the fix.
- CLI flags, exit codes, human-readable output format.

---

## Changes

### 1. `crates/cih-engine/src/db.rs` — add `load_many_to_falkor`

Add a second function that accepts a slice of artifact references. It opens one staging
session, calls `bulk_load` for each artifact set in order (analyze first, community second),
then publishes once. `load_to_falkor` remains unchanged as a thin wrapper.

```rust
/// Load multiple artifact sets into one staging graph, then publish atomically.
/// Callers supply artifacts in the order they should be merged (analyze first, community second).
pub(crate) fn load_many_to_falkor(
    url: &str,
    graph_key: &str,
    artifact_sets: &[&GraphArtifacts],
) -> Result<cih_graph_store::LoadStats> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime")?;

    rt.block_on(async {
        let staging_key = format!("{graph_key}-staging");
        let store = FalkorStore::connect(url, &staging_key)
            .map_err(|e| anyhow::anyhow!("FalkorDB connect: {e}"))?;
        let _ = store.drop_graph().await;
        store
            .ensure_schema()
            .await
            .map_err(|e| anyhow::anyhow!("FalkorDB ensure_schema: {e}"))?;

        let mut total_nodes = 0u64;
        let mut total_edges = 0u64;
        for artifacts in artifact_sets {
            let stats = store
                .bulk_load(artifacts)
                .await
                .map_err(|e| anyhow::anyhow!("FalkorDB bulk_load: {e}"))?;
            total_nodes += stats.nodes;
            total_edges += stats.edges;
        }

        store
            .publish_to(graph_key)
            .await
            .map_err(|e| anyhow::anyhow!("FalkorDB publish: {e}"))?;
        if let Err(err) = store.drop_graph().await {
            tracing::warn!(
                graph = staging_key,
                error = %err,
                "failed to drop FalkorDB staging graph"
            );
        }
        Ok(cih_graph_store::LoadStats {
            nodes: total_nodes,
            edges: total_edges,
        })
    })
}
```

**Why a single staging session matters:** both `bulk_load` calls must run on the same
`FalkorStore` instance before `publish_to` is called. If `publish_to` were called after the
first artifact set, the live graph would contain only analyze data; the community load would
then go to a new staging graph that is never published. Calling `bulk_load` twice on the same
staging store is safe because `load_nodes_edges` uses `MERGE` — there is no overlap between
analyze nodes (classes, methods, fields) and community nodes (Community, Process), and
community `MEMBER_OF` edges reference analyze node IDs that are already present in the staging
graph when the second load runs.

---

### 2. `crates/cih-engine/src/discover.rs` — store source `GraphArtifacts` and use `load_many_to_falkor`

**2a. `DiscoverOutcome`: replace `source_version: String` with `source_artifacts: GraphArtifacts`**

Currently (line 145):
```rust
pub(crate) struct DiscoverOutcome {
    pub(crate) source_version: String,   // ← only saves the version string
    pub(crate) artifacts: GraphArtifacts,
    ...
}
```

Change to:
```rust
pub(crate) struct DiscoverOutcome {
    pub(crate) source_artifacts: GraphArtifacts,   // full analyze GraphArtifacts
    pub(crate) artifacts: GraphArtifacts,           // community-only GraphArtifacts
    ...
}
```

Derive `source_version` wherever it is used:
- `DiscoverOutcome::summary` (line 162): `source_version: self.source_artifacts.version.0.as_str()`
- `DiscoverOutcome::print_human` (line 179): `self.source_artifacts.version.0`

**2b. `run_discover_core`: save full `source` instead of just its version**

Currently (line 117):
```rust
Ok(DiscoverOutcome {
    source_version: source.version.0,   // ← drops the GraphArtifacts
    artifacts,
    ...
})
```

Change to:
```rust
Ok(DiscoverOutcome {
    source_artifacts: source,   // keep the full GraphArtifacts
    artifacts,
    ...
})
```

**2c. `run_discover`: replace `load_to_falkor` with `load_many_to_falkor`**

Currently (line 27):
```rust
match load_to_falkor(url, key, &emit.artifacts) {
```

Change to:
```rust
match load_many_to_falkor(url, key, &[&emit.source_artifacts, &emit.artifacts]) {
```

Update the import at the top of `discover.rs`:
```rust
use crate::db::{load_many_to_falkor, LoadOutcome};
```

The log message at line 29 stays the same shape; the counts will now reflect both artifact
sets combined.

---

### 3. `crates/cih-engine/src/tests.rs` — add two new tests

**Test 1 — `discover_preserves_analyze_artifacts_on_disk`**

Verifies the disk layout invariant: `discover` must not move or delete analyze artifacts.

```rust
#[test]
fn discover_preserves_analyze_artifacts_on_disk() {
    let root = temp_repo();
    write(
        &root,
        "src/main/java/com/example/OwnerService.java",
        "package com.example;\n@Service\nclass OwnerService {\n  public void findAll() { helper(); }\n  private void helper() {}\n}\n",
    );
    let scan = scan::scan_repo(&root).unwrap();
    let analyze = analyze_emit(&scan, all_scope()).unwrap();

    // Save analyze artifact paths before discover runs.
    let analyze_nodes = analyze.artifacts.nodes_path.clone();
    let analyze_edges = analyze.artifacts.edges_path.clone();
    let analyze_version = analyze.artifacts.version.0.clone();

    let discover = run_discover_core(&root).unwrap();

    // Analyze artifacts must still exist on disk.
    assert!(analyze_nodes.exists(), "analyze nodes.jsonl must survive discover");
    assert!(analyze_edges.exists(), "analyze edges.jsonl must survive discover");

    // latest_graph_artifacts must still resolve to the analyze version, not the community one.
    let latest = crate::versioning::latest_graph_artifacts(&root).unwrap();
    assert_eq!(
        latest.version.0, analyze_version,
        "latest_graph_artifacts must still return the analyze version after discover"
    );
    assert!(
        latest.nodes_path.to_string_lossy().contains("artifacts/"),
        "latest_graph_artifacts path must be under .cih/artifacts/, not artifacts-community/"
    );

    // Discover artifacts must be separate, under artifacts-community.
    assert!(
        discover.artifacts.nodes_path
            .to_string_lossy()
            .contains("artifacts-community"),
        "discover artifacts must be under .cih/artifacts-community/"
    );

    fs::remove_dir_all(&root).unwrap();
}
```

**Test 2 — `discover_outcome_source_artifacts_point_to_analyze_dir`**

Verifies `DiscoverOutcome.source_artifacts` holds analyze-side paths (used by the load path).

```rust
#[test]
fn discover_outcome_source_artifacts_point_to_analyze_dir() {
    let root = temp_repo();
    write(
        &root,
        "src/main/java/com/example/OwnerService.java",
        "package com.example;\n@Service\nclass OwnerService {\n  public void findAll() { helper(); }\n  private void helper() {}\n}\n",
    );
    let scan = scan::scan_repo(&root).unwrap();
    analyze_emit(&scan, all_scope()).unwrap();

    let discover = run_discover_core(&root).unwrap();

    // source_artifacts must point into .cih/artifacts/ (analyze side).
    assert!(
        discover.source_artifacts.nodes_path
            .to_string_lossy()
            .contains("artifacts/"),
        "source_artifacts must be under .cih/artifacts/"
    );
    assert!(
        !discover.source_artifacts.nodes_path
            .to_string_lossy()
            .contains("artifacts-community"),
        "source_artifacts must NOT be under .cih/artifacts-community/"
    );

    // community artifacts must point into .cih/artifacts-community/.
    assert!(
        discover.artifacts.nodes_path
            .to_string_lossy()
            .contains("artifacts-community"),
        "discover.artifacts must be under .cih/artifacts-community/"
    );

    // Source and community artifact versions must differ (different content hashes).
    assert_ne!(
        discover.source_artifacts.version.0,
        discover.artifacts.version.0,
        "source and community versions must differ"
    );

    fs::remove_dir_all(&root).unwrap();
}
```

---

## Files changed

| File | Change |
|---|---|
| `crates/cih-engine/src/db.rs` | Add `load_many_to_falkor` |
| `crates/cih-engine/src/discover.rs` | `source_version: String` → `source_artifacts: GraphArtifacts`; update two `impl` methods; switch `run_discover` to `load_many_to_falkor` |
| `crates/cih-engine/src/tests.rs` | Add two new tests |

`analyze.rs`, `db.rs` existing `load_to_falkor`, CLI flags, disk layout — all unchanged.

---

## Implementation order

1. Add `load_many_to_falkor` to `db.rs`. Run `cargo check -p cih-engine`.
2. Update `DiscoverOutcome` struct field in `discover.rs`. Fix the two `impl` methods that
   use `source_version`. Run `cargo check -p cih-engine`.
3. Update `run_discover_core` to store `source_artifacts: source`. Run `cargo check`.
4. Update `run_discover` to call `load_many_to_falkor`. Run `cargo check`.
5. Add the two tests to `tests.rs`. Run `cargo test -p cih-engine`.
6. Run `cargo test --workspace`.

---

## Acceptance criteria

- [ ] `cargo test -p cih-engine discover` — both new tests pass
- [ ] `cargo test -p cih-engine` — all existing tests pass (74 currently)
- [ ] `cargo test --workspace` — no regressions
- [ ] `load_to_falkor` signature is unchanged — zero changes to `analyze.rs` or `resolve` path
- [ ] After `discover`, FalkorDB staging receives analyze nodes first, community nodes second,
      then a single `publish_to` — verified by reading `load_many_to_falkor` code structure
- [ ] `DiscoverSummary.falkor_nodes` and `falkor_edges` report combined counts from both
      artifact sets
