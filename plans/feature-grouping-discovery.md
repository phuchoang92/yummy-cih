# Plan: Feature Grouping as a First-Class Discovery Artifact

## Problem

Feature grouping (mapping classes → domain features like "overdraft", "payment") is
currently hardcoded inside `cih-wiki/src/graph.rs`. It knows about Maven path markers,
Java package conventions, prefix/suffix stripping, and generic layer names. This is the
wrong layer and does not scale:

- `cih-wiki` is a rendering library; it should not know about Java or Maven.
- Different projects have different structures (Go, Kotlin, custom layouts).
- Expensive AI/ML classification runs on every wiki rebuild instead of once.
- There is no way to review, correct, or lock groupings before docs are generated.

The fix: move feature grouping into the `discover` command as a pluggable strategy that
writes a new artifact type. The wiki command reads that artifact; it never derives
features itself.

---

## Target Architecture

```
cih-engine discover /repo --feature-strategy hybrid
  └─ writes .cih/artifacts-features/<graph_ver>/
       groups-package.jsonl   raw package strategy output
       groups-llm.jsonl       raw LLM strategy output (optional)
       groups.jsonl           merged canonical file (wiki reads this)

cih-engine wiki /repo
  └─ reads .cih/artifacts-features/<graph_ver>/groups.jsonl  ← NEW
  └─ falls back to path heuristic if artifact absent          ← backward compat

.cih/feature-overrides.json   human correction sidecar, never auto-generated
```

### New crate: `cih-grouping`

```
crates/
  cih-core/         Node, Edge, NodeId types (unchanged)
  cih-grouping/     NEW — trait + all strategy impls + registry
  cih-engine/       CLI; builds strategy from config, calls cih-grouping
  cih-wiki/         reads groups.jsonl only; zero dep on cih-grouping
```

`cih-wiki` must not depend on `cih-grouping`. It reads the artifact format, nothing more.

---

## Artifact Format

### `.cih/artifacts-features/<graph_ver>/groups.jsonl`

One line per node assignment. The wiki reads this single file.

```jsonc
{"id":"feature:overdraft","name":"overdraft","node_id":"Class:com.bank.overdraft.OverdraftService","strategy":"package","confidence":1.0,"pinned":false,"evidence":"module dir banking-overdraft stripped of prefix+suffix"}
{"id":"feature:overdraft","name":"overdraft","node_id":"Class:com.bank.custom.CustomOverdraftImpl","strategy":"llm","confidence":0.9,"pinned":false,"evidence":"class name + OverdraftRepository calls"}
{"id":"feature:shared","name":"shared","node_id":"Class:com.bank.platform.EventPublisher","strategy":"structural","confidence":1.0,"pinned":false,"evidence":"in-degree spans 4 distinct features"}
```

Rust type (lives in `cih-grouping`, re-exported from `cih-core`):

```rust
pub struct FeatureGroupEntry {
    pub id: String,               // "feature:<slug>"
    pub name: String,             // slug, e.g. "overdraft"
    pub node_id: NodeId,
    pub strategy: String,         // "package" | "llm" | "embed" | "structural" | "override"
    pub confidence: f32,          // 0.0–1.0
    pub pinned: bool,
    pub evidence: String,
    pub node_content_hash: u64,   // FNV-64 of (fqn|file_path|kind) for cache hits
}
```

### `.cih/feature-overrides.json` (human sidecar)

```json
{
  "version": 1,
  "entries": [
    {
      "node_id": "Class:com.bank.custom.CustomOverdraftImpl",
      "feature": "overdraft",
      "reason": "manual correction 2026-06-22"
    }
  ]
}
```

Overrides are injected during the merge step with `strategy:"override"`, `confidence:1.0`,
`pinned:true`. They are never overwritten by any automated run.

### Versioning

`<graph_ver>` is the same `VersionId` as the graph artifact it was derived from — no
separate version counter. When graph re-indexes, features re-run. Pinned overrides are
re-injected into the new version automatically.

---

## The `FeatureStrategy` Trait (`cih-grouping`)

```rust
#[async_trait]
pub trait FeatureStrategy: Send + Sync {
    fn name(&self) -> &str;

    /// Returns (successes, failures). Partial results are always usable.
    async fn run(
        &self,
        input: &StrategyInput<'_>,
        cfg: &StrategyConfig,
    ) -> (Vec<FeatureGroupEntry>, Vec<StrategyError>);
}

pub struct StrategyInput<'a> {
    pub nodes: &'a [Node],
    pub edges: &'a [Edge],
    pub graph_version: &'a str,
}
```

### Strategy registry

```rust
pub fn build_strategy(cfg: &StrategyConfig) -> Box<dyn FeatureStrategy> {
    match cfg.kind {
        StrategyKind::Package    => Box::new(PackageStrategy),
        StrategyKind::Llm        => Box::new(LlmStrategy::from_config(cfg)),
        StrategyKind::Embed      => Box::new(EmbedStrategy::from_config(cfg)),
        StrategyKind::Structural => Box::new(StructuralStrategy),
        StrategyKind::Hybrid     => Box::new(HybridStrategy {
            inner: cfg.hybrid_strategies.iter()
                .map(|k| build_strategy(&StrategyConfig { kind: k.clone(), ..cfg.clone() }))
                .collect(),
            merge: cfg.hybrid_merge.clone(),
        }),
    }
}
```

`HybridStrategy::run` runs inner strategies concurrently, then merges by the configured
rule (`LlmOverridesPackage` / `MaxConfidence` / `Intersection`).

---

## The Four Strategies

### `package` (rule-based, zero cost)

Extracted from `graph.rs` into `cih-grouping/src/strategies/package.rs`. Accepts a
`PackageConfig` with user-overridable prefix/suffix lists and skip lists — no more
hardcoded Rust constants for Java conventions.

```toml
# .cih/grouping.toml (project-local config, optional)
[package_grouping]
src_roots      = ["src/main/java", "src/main/kotlin"]
strip_prefixes = ["banking-", "payment-"]
strip_suffixes = ["-api", "-impl", "-service", "-core"]
catch_all      = ["core", "common", "shared", "custom", "impl"]
skip_segments  = ["service", "repository", "gateway", "controller", "mapper"]
```

When `.cih/grouping.toml` is absent, built-in defaults apply (current behaviour preserved).

### `structural` (rule-based, annotation-driven)

Runs before LLM to detect truly cross-cutting classes cheaply. Marks a node as
`feature:shared` if **two or more** of:
- Annotation is `@Aspect`, or simple name contains `Filter / Interceptor / Listener /
  Audit / Logger / Publisher / Security`
- Call-graph in-degree spans ≥ 3 distinct phase-1 features
- File path contains `platform-core / common / infrastructure / framework`

These get `confidence:1.0` without any API call.

### `embed` (vector similarity, offline)

Reuses `all-minilm-l6-v2` already in the stack. Text per class:

```
{SimpleClassName} {annotation_list} {top5_method_names} {called_class_simple_names}
```

For hybrid residuals: compute the centroid embedding per phase-1 feature cluster, then
assign by cosine similarity > 0.65. Below threshold → escalate to LLM. Full clustering
(HDBSCAN) only when running `--feature-strategy embed` standalone.

### `llm` (LLM classification)

**Input evidence per class** (ranked by signal):

1. Simple class name (decode camelCase — most reliable)
2. Spring stereotype annotations (`@RestController`, `@Service`, `@Repository`,
   `@FeignClient`, `@KafkaListener`) — already in `Node.props`
3. Top-5 method names
4. Feature labels of called classes (from phase-1 output, not raw class names)
5. DB tables accessed via `ReadsTable`/`WritesTable` edges
6. File path module segment

**Candidate features list** comes from phase-1 package strategy output — the LLM must
classify within the established vocabulary. This prevents label drift between phases.

**Prompt (user message):**

```
Candidate features: ["payment", "overdraft", "auth", "order", "shared"]

Classify each class below. Output one JSON object per line:
{"id":"<node_id>","feature":"<feature>","confidence":"high|medium|low","reason":"<phrase>"}

Classes:
---
id: Class:com.bank.custom.CustomOverdraftImpl
name: CustomOverdraftImpl
annotations: @Service
methods: calculateOverdraftFee, applyOverdraftCharge, getOverdraftLimit
calls_into_features: overdraft, account
file_module: custom-impl
---
```

**Batch size:** 15–20 classes per call. Reuses the existing `LlmAdapter` trait and
`backoff_ms` retry loop from `cih-engine`.

**Confidence mapping:**
- `"high"` → 0.9, `"medium"` → 0.7, `"low"` → 0.4
- Low-confidence results with cross-cutting name patterns fall back to `structural` /
  "shared" — no multi-label needed.

---

## Hybrid Flow (recommended default)

```
1. structural  — tag obvious cross-cutting nodes as "shared"          (free)
2. package     — assign high-confidence nodes from file paths          (free)
3. embed       — assign residuals by centroid cosine similarity        (local, fast)
4. llm         — assign remaining residuals below embed threshold      (API call)
5. overrides   — inject .cih/feature-overrides.json, pinned=true       (free)
6. merge       — produce groups.jsonl, LlmOverridesPackage rule        (free)
```

Residuals = nodes assigned to "shared" or "custom/common/impl" catch-alls by the package
strategy. Only these go to embed/LLM — the bulk of the codebase never touches an API.

---

## CLI Changes

```
cih-engine discover /repo [existing flags...]
  --feature-strategy <package|llm|embed|hybrid>   [default: package]
  --feature-llm-provider <gemini|anthropic|...>
  --feature-llm-model <name>
  --feature-llm-concurrency <n>                   [default: 4]
  --feature-embed-model <name>                    [default: all-minilm-l6-v2]
  --feature-no-cache                              force re-classify all nodes
  --feature-config <path>                         .cih/grouping.toml override
```

`cih-engine wiki` gains no new flags — it reads the artifact automatically.

---

## Backward Compatibility

- If `.cih/artifacts-features/` does not exist, `wiki` falls back to `feature_from_file_path`
  heuristic (current behaviour, unchanged).
- `--grouping package` on the wiki command still works; it bypasses the artifact and runs
  the heuristic inline — useful for quick previews without a full `discover` run.
- All existing tests pass unchanged.

---

## Implementation Phases

### Phase 1 — Extract and externalise (no new behaviour)
- Create `crates/cih-grouping/` with `FeatureStrategy` trait and `FeatureGroupEntry` type
- Move `feature_from_file_path` + helpers out of `graph.rs` into `PackageStrategy`
- Replace hardcoded constants with a `PackageConfig` struct loadable from `.cih/grouping.toml`
- Wire `wiki --grouping package` to call `PackageStrategy` via the new trait
- All existing tests must still pass

### Phase 2 — Artifact write/read
- `discover` writes `groups.jsonl` after `PackageStrategy::run`
- `wiki` reads `groups.jsonl` when present; passes resolved `Vec<FeatureGroupEntry>` to
  `build_package_grouped` (new optional param) instead of deriving features inline
- Implement `feature-overrides.json` sidecar injection during merge

### Phase 3 — Structural + embed strategies
- `StructuralStrategy`: annotation + in-degree cross-cutting detection
- `EmbedStrategy`: reuse `all-minilm-l6-v2`, centroid cosine similarity for residuals
- `HybridStrategy` composer

### Phase 4 — LLM strategy
- `LlmStrategy`: batch prompt, response parser, confidence mapping
- Wire into `HybridStrategy` as the final residual handler
- Incremental cache: skip nodes whose `node_content_hash` matches prior run

### Phase 5 — Tooling
- `cih-engine features show /repo` — print current groupings table
- `cih-engine features override /repo <node_id> <feature>` — write to sidecar
- `cih status` shows pinned count, strategy used, last-run version

---

## Key Open Questions

| Question | Recommendation |
|---|---|
| Does `FeatureGroupEntry` live in `cih-core` or `cih-grouping`? | `cih-grouping`; re-export from `cih-wiki` as a shim. Avoids bloating `cih-core`. |
| Async runtime in `cih-grouping`? | `tokio` feature-gated; `PackageStrategy` is sync, trait is async. |
| HDBSCAN library in Rust? | `hdbscan` crate or port; evaluate at Phase 3. k-means as interim fallback. |
| Multi-module projects with no `.cih/grouping.toml`? | Phase 1 built-in defaults cover the common Java/Maven cases. Config file is optional. |
| What if phase-1 produces >20 feature labels? | Cap candidate list at 15 for LLM prompts; merge rare labels into nearest semantic neighbour. |
