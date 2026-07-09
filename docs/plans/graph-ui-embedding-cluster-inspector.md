# Plan: Inspect embedding clusters (feature groups) in the graph UI

## Context

The user ran embedding-based clustering in yummy-cih (`cih-engine discover
--feature-strategy embed`) and wants to **display the clusters and drill into the member
nodes of each** to verify grouping quality — reusing the existing graph UI rather than a
new CLI/report.

Key finding from exploration — there are **two distinct clustering concepts**, and the UI
currently only shows the wrong one:

- **Communities** (what the UI "Communities" tab shows today): built by
  `--community-strategy package|graph` in `discover.rs:168-210` via `cih-community`
  (`detect_communities` / `detect_communities_from_packages`). These are **structural /
  package** clusters — **no embeddings**. They become `Community` nodes + `MEMBER_OF` edges
  in FalkorDB (MemberOf confidence hardcoded to `1.0`, `cih-community/src/lib.rs:409,716`).
- **Feature groups / embedding clusters** (what the user actually generated): produced by
  the `embed` feature strategy (semantic k-NN + Leiden), written to
  `<repo>/.cih/artifacts-features/<graph_ver>/groups.jsonl` (merged) and `groups-embed.jsonl`
  (raw). Each record is a per-node assignment carrying the quality signals we want:
  `confidence` (cosine similarity to cluster centroid, 0–1) and `evidence`
  (`FeatureGroupEntry` in `crates/cih-grouping/src/entry.rs`). These are **not** surfaced in
  the graph UI at all today.

So the task is: add an **"Clusters"** view to the graph UI backed by the `groups.jsonl`
artifact, letting the user list embedding clusters and inspect each cluster's members sorted
by confidence (low-confidence outliers first) to eyeball grouping quality. The change is
purely additive and read-only — no changes to the indexing pipeline or graph semantics.

## Immediate workaround (works today, no code)

While the feature lands, the user can already inspect clusters:

- Summary table: `cih-engine features show <repo>` (per-cluster node counts / strategy;
  `crates/cih-engine/src/cmd/features.rs`). Note: it does **not** list members — that's the gap.
- Raw members with confidence/evidence: read
  `<repo>/.cih/artifacts-features/<ver>/groups-embed.jsonl` (one JSON record per node:
  `{name, node_id, confidence, evidence, strategy}`), e.g. group by `name`, sort by
  `confidence` ascending to find weakly-attached outliers.

## Approach

Surface `groups.jsonl` through the existing browser HTTP API + React graph UI. No graph
join is needed — `node_id` already encodes `Kind:fqn`, and the UI already parses that (see
`shortLabel` / `KIND_COLORS` in `ClassicViews.tsx`). Selecting a member reuses the existing
`context(id)` path to open its full detail.

### Backend — `crates/cih-server/`

1. **`browser.rs`** — add route `GET /api/graph/features` (handler `graph_features`).
   - Resolve the feature-artifact dir from the server's known artifacts dir. `BrowserState`
     currently holds only `store` + `search` (`browser.rs:29-39`); add a
     `feature_dir: Option<PathBuf>` field, populated in `main.rs` where `BrowserState::new`
     is called (`main.rs:687`) by deriving it from `cfg.artifacts_dir` (which points at
     `.cih/artifacts/<ver>`): `<ver>` and `.cih` are its ancestors, so the sibling is
     `.cih/artifacts-features/<ver>`. Prefer the existing helper
     `cih_grouping::find_feature_artifact_dir(repo, graph_ver)` (`cih-grouping/src/artifact.rs:44`)
     if the repo root + version are threaded through; otherwise derive the sibling path.
   - Read + parse with `cih_grouping::read_feature_artifact(dir)`
     (`cih-grouping/src/artifact.rs:36`) which returns `Vec<FeatureGroupEntry>`. Add a
     `cih-grouping` dependency to `cih-server/Cargo.toml` (or, to avoid the dep, parse the
     JSONL locally into a small serde struct — reuse whichever is lighter).
   - Group entries by `name`; for each cluster emit
     `{ name, node_count, avg_confidence, members: [{ node_id, confidence, evidence,
     strategy, pinned }] }` with `members` sorted by `confidence` ascending (outliers first)
     and clusters sorted by `node_count` descending. Return `{ clusters: [...] }` as JSON.
   - Mirror the read-only, bounded style and `BrowserError` handling of the neighbouring
     handlers (`graph_communities` at `browser.rs:248`, `read_community_nodes` in
     `resources.rs:133` is the precedent for reading artifact JSONL off disk).

### Frontend — `graph-ui/` (React; **builds into** `crates/cih-server/assets/graph/app.js`)

The React app is the canonical UI — `vite build` (per `graph-ui/vite.config.ts`) emits
`app.js`/`styles.css` into `crates/cih-server/assets/graph/`, which the server compiles in
via `include_str!` (`browser.rs:25-27`).

2. **`api.ts`** — add `features: () => fetchJson<any>("/api/graph/features")` (next to
   `communities` at `api.ts:34`).
3. **`types.ts`** — add `"clusters"` to the `TabId` union.
4. **Tab nav** (wherever tabs are rendered — `App.tsx`) — add a "Clusters" tab.
5. **`ClassicViews.tsx`** — handle `tab === "clusters"`:
   - On mount, call `api.features()`; render clusters in the left `result-rail` (reuse the
     existing rail markup at `ClassicViews.tsx:84`), showing per-cluster `name`,
     `node_count`, and `avg_confidence`.
   - Selecting a cluster shows its **members** panel: each member row shows the derived kind
     (via `node_id` → `KIND_COLORS`), short label, and a **confidence badge** (percent);
     flag low-confidence rows (e.g. `< 0.5`) so weakly-grouped nodes stand out. Members are
     already sorted ascending by confidence from the backend.
   - Clicking a member calls the existing `onSelectedId(node_id)` so its context opens in the
     detail view — no new detail plumbing needed.
   - Keep it read-only and consistent with the `communities`/`routes` tab patterns already in
     this file.

## Files to modify

- `crates/cih-server/src/browser.rs` — new `/api/graph/features` route + handler +
  `feature_dir` on `BrowserState`.
- `crates/cih-server/src/main.rs` — populate `feature_dir` at `BrowserState::new` (~`main.rs:687`).
- `crates/cih-server/Cargo.toml` — add `cih-grouping` dep (unless parsing JSONL locally).
- `graph-ui/src/api.ts`, `graph-ui/src/types.ts`, `graph-ui/src/App.tsx`,
  `graph-ui/src/ClassicViews.tsx` — new "Clusters" tab + members panel.

## Reuse (don't re-implement)

- `cih_grouping::read_feature_artifact` / `find_feature_artifact_dir` /
  `feature_artifact_dir` — `crates/cih-grouping/src/artifact.rs`.
- `FeatureGroupEntry` (`id, name, node_id, strategy, confidence, pinned, evidence`) —
  `crates/cih-grouping/src/entry.rs`.
- Artifact-off-disk read precedent — `resources.rs:133 read_community_nodes`.
- UI rail + node-kind rendering — `ClassicViews.tsx` (`result-rail`, `shortLabel`,
  `KIND_COLORS`, `idOf`).

## Verification (end-to-end)

1. Ensure an embed run exists for a target repo:
   `cih-engine embed <repo> --pg-url ...` then
   `cih-engine discover <repo> --feature-strategy embed --pg-url ...`; confirm
   `<repo>/.cih/artifacts-features/<ver>/groups.jsonl` exists with `"strategy":"embed"` rows.
   (Local infra reminder: FalkorDB on **6380**, Postgres on **5433**.)
2. Backend: `cargo test --workspace` (hermetic). Add a unit test for the grouping/sort logic
   in the new handler using an artifact fixture (mirror existing `cih-server/tests` style).
3. Frontend: `cd graph-ui && npm run build` (emits into the server assets), plus
   `npm test` for any `ClassicViews`/`api` test touched.
4. Manual: run the server against the indexed repo, open `/graph`, select the **Clusters**
   tab. Confirm clusters list with sizes + avg confidence; open a cluster and confirm members
   are sorted with low-confidence outliers at the top and confidence badges shown; click a
   member and confirm its context opens. Sanity-check a couple of clusters against the raw
   `groups.jsonl` to confirm membership matches.

## Notes

- Per repo convention (`yummy-cih/CLAUDE.md`), on execution also drop a copy of this plan in
  `yummy-cih/docs/plans/`.
- Scope is intentionally read-only/additive. If later we want the *structural* communities
  tab to also carry real per-member confidence, that's a separate change (propagate
  `FeatureGroupEntry.confidence` onto `MEMBER_OF` edges, which are currently `1.0`).
