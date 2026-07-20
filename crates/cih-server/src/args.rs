use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Traversal direction for impact analysis. An unknown string fails
/// deserialization (it used to silently run `upstream`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirectionArg {
    /// Callers / blast radius (default).
    #[default]
    Upstream,
    /// Callees.
    Downstream,
    /// Both directions.
    Both,
}

/// Scope of the git diff for `detect_changes`. An unknown string fails
/// deserialization (it used to silently run `working`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiffScope {
    /// All uncommitted changes vs HEAD.
    Working,
    /// Index vs HEAD.
    Staged,
    /// HEAD vs a branch/commit — requires the `base_ref` argument.
    BaseRef,
}

/// Output format for `impact`. A typo fails deserialization (it used to
/// silently fall back to JSON); the empty string stays accepted because the
/// tool docs long said "pass empty for default".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImpactFormat {
    /// Default JSON impact report.
    #[default]
    #[serde(alias = "")]
    Json,
    /// D3 force-directed diagram JSON.
    Diagram,
}

/// Output format for `communities` (same contract as [`ImpactFormat`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommunitiesFormat {
    /// Default JSON community list.
    #[default]
    #[serde(alias = "")]
    Json,
    /// D3 service-map diagram JSON.
    Diagram,
}

/// Output format for `route_map` (same contract as [`ImpactFormat`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RouteMapFormat {
    /// Default JSON route list.
    #[default]
    #[serde(alias = "")]
    Json,
    /// OpenAPI 3.0.3 JSON.
    Openapi,
}

/// Output format for `trace_flow` (same contract as [`ImpactFormat`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TraceFlowFormat {
    /// Default JSON step list.
    #[default]
    #[serde(alias = "")]
    Json,
    /// Mermaid flowchart string.
    Mermaid,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContextArgs {
    /// Symbol id (e.g. `Method:com.acme.UserService#save`) or short name
    /// (e.g. `UserService`). Short names trigger disambiguation.
    pub name: String,
    /// Target service: a group member's registry name (e.g. "212ecom-ai");
    /// empty = the server's primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImpactArgs {
    /// Symbol id or short name to analyze.
    pub name: String,
    /// `upstream` (callers / blast radius, default), `downstream`, or `both`.
    #[serde(default)]
    pub direction: DirectionArg,
    /// Max traversal depth (default 4, pass 0 for default).
    #[serde(default)]
    pub max_depth: u32,
    /// Output format. Omit or pass empty for default JSON. Pass `"diagram"` for D3 force-directed JSON.
    #[serde(default)]
    pub format: ImpactFormat,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CommunitiesArgs {
    /// Maximum number of communities to return (0 = all).
    #[serde(default)]
    pub limit: usize,
    /// Output format. Omit or pass empty for default JSON. Pass `"diagram"` for D3 service-map JSON.
    #[serde(default)]
    pub format: CommunitiesFormat,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RouteMapArgs {
    /// Path prefix filter (e.g. "/api/owners"). Omit or leave empty for all routes.
    #[serde(default)]
    pub prefix: String,
    /// Max routes to return (default 200, pass 0 for default).
    #[serde(default)]
    pub limit: usize,
    /// Output format. Omit or pass empty for default JSON. Pass `"openapi"` for OpenAPI 3.0.3 JSON.
    #[serde(default)]
    pub format: RouteMapFormat,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatusArgs {
    /// Repo name or absolute path as shown in `list_repos`.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DetectChangesArgs {
    /// Scope of the git diff: `working` (all uncommitted vs HEAD),
    /// `staged` (index vs HEAD), or `base_ref` (HEAD vs a branch/commit).
    pub scope: DiffScope,
    /// Git ref for `base_ref` scope (e.g. `main` or a commit SHA). Leave empty for non-base_ref scopes.
    #[serde(default)]
    pub base_ref: String,
    /// Repo name or absolute path (from registry). Leave empty to use the server's active graph key.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GroupContractsArgs {
    /// Group name created with `cih-engine group create`.
    pub group: String,
    /// Optional kind filter: `all`, `http`, `http_route`, `kafka`, `kafka_topic`,
    /// `spring`, or `spring_event`. Leave empty for all.
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApiImpactArgs {
    /// Group name created with `cih-engine group create`.
    pub group: String,
    /// HTTP method: GET, POST, PUT, DELETE, PATCH (case-insensitive).
    pub method: String,
    /// Route path template, e.g. `/api/orders/{id}`. Path variables are normalized to `{*}`.
    pub path: String,
    /// Also walk each consumer's callers (reverse CALLS from the call site,
    /// with the enclosing route when one handles the caller).
    #[serde(default)]
    pub include_callers: bool,
    /// Reverse-CALLS depth for the caller walk (default 3, clamped to 6, pass 0 for default).
    #[serde(default)]
    pub caller_depth: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShapeCheckArgs {
    /// Group name created with `cih-engine group create`.
    pub group: String,
    /// Provider repo name (as registered with `cih-engine analyze`).
    pub provider: String,
    /// Consumer repo name (as registered).
    pub consumer: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceFlowArgs {
    /// Symbol id or short name to trace from. Accepts a Route node
    /// (e.g. `Route:GET /api/checkout`) or a Method node id.
    /// Short names trigger disambiguation like `context` and `impact`.
    pub entry_point: String,
    /// Maximum traversal depth (default 6, clamped to 10, pass 0 for default).
    #[serde(default)]
    pub max_depth: u32,
    /// Output format. Omit or pass empty for default JSON. Pass `"mermaid"` for a Mermaid flowchart string.
    #[serde(default)]
    pub format: TraceFlowFormat,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceFlowXArgs {
    /// Entry node in the start repo: a full node id
    /// (e.g. `Route:GET /api/checkout`, `Method:pkg.Cls#m/1`) or a unique
    /// name/qualified name (ambiguity returns candidates).
    pub entry_point: String,
    /// Repo name or absolute path (from registry) to start the trace in.
    /// Leave empty to use the server's active graph key. Must be a member of `group`.
    #[serde(default)]
    pub repo: String,
    /// Repo group whose synced contracts bridge the repos
    /// (`cih-engine group sync <group>` must have run).
    pub group: String,
    /// Per-repo traversal depth (default 6, clamped to 10, pass 0 for default).
    #[serde(default)]
    pub max_depth: u32,
    /// Maximum cross-repo contract hops (default 3, clamped to 5, pass 0 for default).
    #[serde(default)]
    pub max_hops: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FeatureMapArgs {
    /// Business keywords to map to code clusters (e.g. "checkout payment").
    pub query: String,
    /// Max symbols to search for before clustering (default 50, max 200, pass 0 for default).
    #[serde(default)]
    pub limit: usize,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchCodeArgs {
    /// Natural language or keyword query (e.g. "rate limiting", "payment settlement timeout").
    pub query: String,
    /// Maximum number of results to return (default 10, max 50, pass 0 for default).
    #[serde(default)]
    pub limit: usize,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Serialize)]
pub struct CodeMatch {
    pub node_id: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: Option<String>,
    pub file: String,
    pub line: u32,
    pub score: f32,
    pub rank: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TestCoverageArgs {
    /// Symbol to look up test coverage for — full NodeId or short name.
    pub name: String,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegressionScopeArgs {
    /// Repo-relative file paths that changed (e.g. ["src/main/java/com/acme/OrderService.java"]).
    pub changed_files: Vec<String>,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UntestedPathsArgs {
    /// Repo-relative path prefix to restrict the search (e.g. "src/main/java/com/acme/payment").
    pub module_prefix: String,
    /// Max symbols to return (default 50, max 500, pass 0 for default).
    #[serde(default)]
    pub limit: usize,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IndexRepoArgs {
    /// Absolute path to the repository to index (e.g. "/home/user/my-service").
    pub repo_path: String,
    /// Languages to index, comma-separated (e.g. "java,typescript"). Leave empty for all detected.
    #[serde(default)]
    pub languages: String,
    /// Graph key to load the repo under. Leave empty for an already-registered
    /// repo (its registry key is used). Required for a path not yet in the
    /// registry; a key owned by a different repo is rejected.
    #[serde(default)]
    pub graph_key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IndexStatusArgs {
    /// Job ID returned by `index_repo`.
    pub job_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddResolvePatternArgs {
    /// Repo name or absolute path. Empty = the server's active graph.
    #[serde(default)]
    pub repo: String,
    /// Rule kind. Currently only "route" is supported.
    #[serde(default = "default_route_kind")]
    pub kind: String,
    /// Annotation simple name to match on a method (no `@`), e.g. "BankEndpoint".
    pub annotation: String,
    /// Annotation attribute holding the URL path. Empty = "value" (the positional arg).
    #[serde(default)]
    pub path_attr: String,
    /// Fixed HTTP method for every match, e.g. "POST". Use this OR method_attr; empty defaults to GET.
    #[serde(default)]
    pub method: String,
    /// Annotation attribute holding the HTTP method, when it varies per usage.
    #[serde(default)]
    pub method_attr: String,
    /// Optional class-level annotation whose value is a path prefix (e.g. "BankResource").
    #[serde(default)]
    pub class_prefix_annotation: String,
    /// Re-index the repo after adding so the live graph reflects the new pattern (default true).
    #[serde(default = "default_true")]
    pub reindex: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListResolvePatternsArgs {
    /// Repo name or absolute path. Empty = the server's active graph.
    #[serde(default)]
    pub repo: String,
}

fn default_route_kind() -> String {
    "route".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Repo-relative file path as returned by search_code or context (e.g.
    /// "src/main/java/com/acme/OrderService.java").
    pub path: String,
    /// Repo name or absolute path (from registry). Leave empty to use the server's active repo.
    #[serde(default)]
    pub repo: String,
    /// First line to return, 1-based inclusive (default: 1, pass 0 for default).
    #[serde(default)]
    pub start_line: u32,
    /// Last line to return, 1-based inclusive (default: entire file, pass 0 for default).
    #[serde(default)]
    pub end_line: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepFilesArgs {
    /// Regex pattern to search for (e.g. "TODO|FIXME", "^import ", "@Deprecated").
    /// Prefix with (?i) for case-insensitive matching.
    pub pattern: String,
    /// Glob to filter files (e.g. "**/*.java", "src/**/*.rs").
    /// Leave empty to search all non-ignored files.
    #[serde(default)]
    pub glob: String,
    /// Repo name or absolute path (from registry). Leave empty to use the server's active repo.
    #[serde(default)]
    pub repo: String,
    /// Max matches to return (default 200, capped at 1000, pass 0 for default).
    #[serde(default)]
    pub limit: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListReposArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComplexityHotspotsArgs {
    /// Minimum cyclomatic complexity to include (0 = use server default of 5).
    #[serde(default)]
    pub min_cyclomatic: u16,
    /// Minimum cognitive complexity to include (0 = use server default of 0).
    #[serde(default)]
    pub min_cognitive: u16,
    /// Minimum transitive loop depth to include (0 = use server default of 1).
    #[serde(default)]
    pub min_transitive_loop: u8,
    /// Maximum number of results (default: 20, max 200, pass 0 for default).
    #[serde(default)]
    pub limit: usize,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaintPathsArgs {
    /// Sink category filter: `all` (default), `sql` (SQL injection), `exec`
    /// (OS command execution), `file` (unsafe file write), or `html` (XSS).
    #[serde(default)]
    pub category: String,
    /// Minimum confidence to include, 0.0–1.0 (default 0.5). Pass an explicit
    /// 0.0 to include every candidate path.
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f32,
    /// Run refinement phases 1–3 (intra-procedural liveness, CFG, PDG
    /// flow-sensitive taint) to adjust confidence. Slower — reads source files
    /// for methods on candidate paths. Default: false (Phase 0 BFS only).
    #[serde(default)]
    pub refine: bool,
    /// Max paths to return (default 50, max 500, pass 0 for default).
    #[serde(default)]
    pub limit: usize,
    /// Repo name or absolute path (from registry). Leave empty to use the server's active graph key.
    #[serde(default)]
    pub repo: String,
}

fn default_min_confidence() -> f32 {
    0.5
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindDuplicatesArgs {
    /// Symbol id or short name of the method to find near-duplicates for.
    pub name: String,
    /// Minimum Jaccard similarity threshold (default: 0.95, pass 0.0 for default).
    #[serde(default)]
    pub min_jaccard: f32,
    /// Maximum number of results (default: 10, pass 0 for default).
    #[serde(default)]
    pub limit: usize,
    /// Target service: a group member's registry name; empty = primary repo.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchWikiArgs {
    /// Search query (keywords or natural language) over generated wiki pages.
    pub query: String,
    /// Repo name or absolute path (from registry). Leave empty to use the server's active graph key.
    #[serde(default)]
    pub repo: String,
    /// Feature/module facet (the manifest `role` grouping, e.g. `loan`, `system`, `shared`). Leave empty for all.
    #[serde(default)]
    pub role: String,
    /// Page kind facet — persona pages carry their persona as the kind: `po`, `ba`, `dev`,
    /// plus `index`, `routes`, `api-flow`. Leave empty for all kinds.
    #[serde(default)]
    pub kind: String,
    /// Feature facet: matches a hit's `community_id`. Leave empty for all features.
    #[serde(default)]
    pub feature: String,
    /// Max hits to return (default 20, max 50, pass 0 for default).
    #[serde(default)]
    pub limit: usize,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetWikiPageArgs {
    /// Page slug from search_wiki hits (e.g. `fineract-provider/dev/loan-service`).
    pub slug: String,
    /// Repo name or absolute path (from registry). Leave empty to use the server's active graph key.
    #[serde(default)]
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArchitectureOverviewArgs {
    /// Sections to include. Empty = default set (`stats`, `modules`,
    /// `route_groups`, `entrypoints`, `wiki_pages`). `hotspots` is opt-in.
    #[serde(default)]
    pub sections: Vec<String>,
    /// Max items per list section (0 = per-section defaults; clamped to 100).
    #[serde(default)]
    pub limit: usize,
    /// Target repo: a registry name from `list_repos`; empty = the server's primary repo.
    #[serde(default)]
    pub repo: String,
}
