//! doc_pack/doc_status behavior, hash-contract, byte-cap, renderer, and scan
//! tests over an in-memory graph store fake.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cih_core::{
    Edge, EdgeKind, GraphArtifacts, GraphDelta, Node, NodeId, NodeKind, Range, RegistryEntry,
    RegistryStats,
};
use cih_graph_store::{
    CallSiteArgs, CommunityEdge, CommunityInfo, ContextPage, ContextSection, DbEffect, Direction,
    FlowEdge, FlowFilter, FlowNode, FlowPage, GraphOverview, GraphStore, GraphStoreError,
    GraphSummary, HotspotNode, Impact, Path as GraphPath, Result as StoreResult, RouteInfo,
    SimilarMethod, Subgraph, SymbolContext, TestCoveragePage, TraversalStats,
};
use cih_search::SearchHit;

use super::*;
use crate::application::change_detection::ChangeDetectionService;
use crate::application::files::{GrepRuntime, ReadFileLimits};
use crate::application::taint::TaintService;
use crate::domain::repository::{RepoCatalogSnapshot, ResolvedRepo};
use crate::ports::artifact_repository::{ArtifactRepository, ArtifactSnapshot};
use crate::ports::changed_files_source::{ChangeScope, ChangedFilesSource};
use crate::ports::cross_repo_graph_provider::{CrossRepoGraph, CrossRepoGraphProvider};
use crate::ports::repo_context_provider::RepoContextProvider;
use crate::ports::search_provider::{SearchProvider, SearchProviderError};

const ROUTE_ID: &str = "Route:GET /api/things";
const HANDLER_ID: &str = "Method:com.acme.Api#getThings/0";
const CALLER_ID: &str = "Method:com.acme.Web#page/0";
const CLASS_ID: &str = "Class:com.acme.Bar";
const MEMBER_ID: &str = "Method:com.acme.Bar#save/1";
const MEMBER_TEST_ID: &str = "Method:com.acme.BarTest#testSave/0";
const FIELD_ID: &str = "Field:com.acme.Bar#state";
const SOURCE_FILE: &str = "src/api.js";

fn node(id: &str, kind: NodeKind, name: &str, file: &str) -> Node {
    Node {
        id: NodeId::new(id),
        kind,
        name: name.to_string(),
        qualified_name: Some(format!("qualified.{name}")),
        file: file.to_string(),
        range: Range {
            start_line: 2,
            end_line: 5,
            ..Range::default()
        },
        props: None,
    }
}

fn flow_hop(
    id: &str,
    kind: NodeKind,
    depth: u32,
    parent: Option<&str>,
    via: Option<&str>,
) -> FlowHop {
    FlowHop {
        node: FlowNode {
            id: NodeId::new(id),
            kind,
            name: id.rsplit(['#', ':']).next().unwrap_or(id).to_string(),
            qualified_name: None,
            file: SOURCE_FILE.to_string(),
            depth,
            parent_id: parent.map(NodeId::new),
            intercepted_by: Vec::new(),
        },
        via: via.map(|kind| FlowEdge {
            kind: kind.to_string(),
            call_sites: vec![CallSiteArgs {
                args: vec!["secret-arg".to_string()],
            }],
        }),
    }
}

/// Canned-data store: only the methods doc_pack touches return data; the rest
/// answer `Unimplemented`. Mutations (`set_*`) let tests move node-local
/// evidence between builds.
#[derive(Default)]
struct FakeStore {
    nodes: Mutex<BTreeMap<String, Node>>,
    flows: Mutex<BTreeMap<String, FlowPage>>,
    db_effects: Mutex<Vec<DbEffect>>,
    callers: Mutex<BTreeMap<String, Vec<Node>>>,
    processes: Mutex<Vec<String>>,
    tests: Mutex<BTreeMap<String, TestCoveragePage>>,
    fail_context: std::sync::atomic::AtomicBool,
    context_calls: AtomicUsize,
    get_node_calls: AtomicUsize,
}

impl FakeStore {
    fn insert_node(&self, node: Node) {
        self.nodes
            .lock()
            .unwrap()
            .insert(node.id.as_str().to_string(), node);
    }

    fn set_flow(&self, entry: &str, page: FlowPage) {
        self.flows.lock().unwrap().insert(entry.to_string(), page);
    }

    fn set_callers(&self, id: &str, callers: Vec<Node>) {
        self.callers.lock().unwrap().insert(id.to_string(), callers);
    }

    fn set_tests(&self, id: &str, page: TestCoveragePage) {
        self.tests.lock().unwrap().insert(id.to_string(), page);
    }
}

fn unimpl<T>() -> StoreResult<T> {
    Err(GraphStoreError::Unimplemented("doc_pack test store"))
}

#[async_trait]
impl GraphStore for FakeStore {
    async fn ensure_schema(&self) -> StoreResult<()> {
        Ok(())
    }

    async fn bulk_load(
        &self,
        _artifacts: &GraphArtifacts,
    ) -> StoreResult<cih_graph_store::LoadStats> {
        unimpl()
    }

    async fn upsert_incremental(&self, _delta: &GraphDelta) -> StoreResult<()> {
        unimpl()
    }

    async fn publish_to(&self, _dest_key: &str) -> StoreResult<()> {
        unimpl()
    }

    async fn drop_graph(&self) -> StoreResult<()> {
        unimpl()
    }

    async fn get_node(&self, id: &NodeId) -> StoreResult<Option<Node>> {
        self.get_node_calls.fetch_add(1, Ordering::Relaxed);
        Ok(self.nodes.lock().unwrap().get(id.as_str()).cloned())
    }

    async fn neighbors(
        &self,
        _id: &NodeId,
        _dir: Direction,
        _kinds: &[EdgeKind],
    ) -> StoreResult<Vec<Edge>> {
        unimpl()
    }

    async fn call_chain(
        &self,
        _from: &NodeId,
        _to: &NodeId,
        _max_depth: u32,
    ) -> StoreResult<Vec<GraphPath>> {
        unimpl()
    }

    async fn subgraph(&self, _seeds: &[NodeId], _radius: u32) -> StoreResult<Subgraph> {
        unimpl()
    }

    async fn graph_summary(&self) -> StoreResult<GraphSummary> {
        unimpl()
    }

    async fn graph_overview(
        &self,
        _max_nodes: usize,
        _max_edges: usize,
        _kinds: Option<&[String]>,
    ) -> StoreResult<GraphOverview> {
        unimpl()
    }

    async fn context(&self, _id: &NodeId) -> StoreResult<SymbolContext> {
        unimpl()
    }

    async fn context_page(
        &self,
        id: &NodeId,
        filter: &cih_graph_store::ContextFilter,
    ) -> StoreResult<ContextPage> {
        self.context_calls.fetch_add(1, Ordering::Relaxed);
        if self.fail_context.load(Ordering::Relaxed) {
            return Err(GraphStoreError::Backend(
                "intentional context outage".into(),
            ));
        }
        let node = self
            .nodes
            .lock()
            .unwrap()
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| GraphStoreError::NotFound(id.as_str().to_string()))?;
        let callers = self
            .callers
            .lock()
            .unwrap()
            .get(id.as_str())
            .cloned()
            .unwrap_or_default();
        let has_more = callers.len() > filter.caller_limit;
        let mut callers = callers;
        callers.truncate(filter.caller_limit);
        Ok(ContextPage {
            node,
            callers: ContextSection {
                items: callers,
                has_more,
                next: None,
            },
            callees: ContextSection {
                items: Vec::new(),
                has_more: false,
                next: None,
            },
            processes: ContextSection {
                items: self.processes.lock().unwrap().clone(),
                has_more: false,
                next: None,
            },
            community: None,
        })
    }

    async fn communities(&self) -> StoreResult<Vec<CommunityInfo>> {
        unimpl()
    }

    async fn route_map(&self, _prefix: Option<&str>, _limit: usize) -> StoreResult<Vec<RouteInfo>> {
        unimpl()
    }

    async fn candidates_by_name(&self, name: &str, limit: usize) -> StoreResult<Vec<Node>> {
        let mut hits: Vec<Node> = self
            .nodes
            .lock()
            .unwrap()
            .values()
            .filter(|node| node.name == name)
            .cloned()
            .collect();
        hits.truncate(limit);
        Ok(hits)
    }

    async fn nodes_in_files(&self, _files: &[String], _limit: usize) -> StoreResult<Vec<Node>> {
        unimpl()
    }

    async fn processes_for_symbols(&self, _ids: &[NodeId]) -> StoreResult<Vec<String>> {
        unimpl()
    }

    async fn flow_downstream(&self, entry: &NodeId, filter: &FlowFilter) -> StoreResult<FlowPage> {
        let mut page = self
            .flows
            .lock()
            .unwrap()
            .get(entry.as_str())
            .cloned()
            .ok_or_else(|| GraphStoreError::NotFound(entry.as_str().to_string()))?;
        let limit = filter.effective_limit();
        if page.hops.len() > limit {
            page.hops.truncate(limit);
            page.has_more = true;
        }
        Ok(page)
    }

    async fn db_effects_for_methods(&self, _ids: &[NodeId]) -> StoreResult<Vec<DbEffect>> {
        Ok(self.db_effects.lock().unwrap().clone())
    }

    async fn complexity_hotspots(
        &self,
        _min_cyclomatic: Option<u16>,
        _min_cognitive: Option<u16>,
        _min_transitive_loop: Option<u8>,
        _limit: usize,
    ) -> StoreResult<Vec<HotspotNode>> {
        unimpl()
    }

    async fn similar_methods(
        &self,
        _id: &NodeId,
        _min_jaccard: f32,
        _limit: usize,
    ) -> StoreResult<Vec<SimilarMethod>> {
        unimpl()
    }

    async fn symbol_communities(
        &self,
        _ids: &[NodeId],
    ) -> StoreResult<Vec<(NodeId, CommunityInfo)>> {
        unimpl()
    }

    async fn test_coverage(&self, _id: &NodeId) -> StoreResult<Vec<Node>> {
        unimpl()
    }

    async fn test_coverage_page(&self, id: &NodeId, limit: usize) -> StoreResult<TestCoveragePage> {
        let page = self
            .tests
            .lock()
            .unwrap()
            .get(id.as_str())
            .cloned()
            .unwrap_or(TestCoveragePage {
                tests: Vec::new(),
                has_more: false,
            });
        // Emulate the backend probe contract: at most `limit` rows returned.
        let mut page = page;
        if page.tests.len() > limit {
            page.tests.truncate(limit);
            page.has_more = true;
        }
        Ok(page)
    }

    async fn tests_for_files(&self, _files: &[String]) -> StoreResult<Vec<Node>> {
        unimpl()
    }

    async fn untested_symbols(&self, _file_prefix: &str, _limit: usize) -> StoreResult<Vec<Node>> {
        unimpl()
    }

    async fn community_graph(&self) -> StoreResult<Vec<CommunityEdge>> {
        unimpl()
    }

    async fn impact(&self, _id: &NodeId, _dir: Direction, _max_depth: u32) -> StoreResult<Impact> {
        unimpl()
    }
}

struct EmptySearch;

#[async_trait]
impl SearchProvider for EmptySearch {
    async fn query_hits(
        &self,
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<SearchHit>, SearchProviderError> {
        Ok(Vec::new())
    }
}

struct EmptyChangedFiles;

impl ChangedFilesSource for EmptyChangedFiles {
    fn changed_files(
        &self,
        _repo_path: &str,
        _scope: ChangeScope,
        _base_ref: Option<&str>,
    ) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

struct UnusedArtifacts;

#[async_trait]
impl ArtifactRepository for UnusedArtifacts {
    async fn snapshot(&self, _repo: &ResolvedRepo) -> Result<Arc<ArtifactSnapshot>, AppError> {
        Err(AppError::Unavailable {
            dependency: "artifacts",
            message: "not used in doc_pack tests".into(),
            retryable: false,
        })
    }

    async fn indexed_snapshot(
        &self,
        _repo: &ResolvedRepo,
    ) -> Result<Arc<ArtifactSnapshot>, AppError> {
        Err(AppError::Unavailable {
            dependency: "artifacts",
            message: "not used in doc_pack tests".into(),
            retryable: false,
        })
    }

    fn invalidate_repo(&self, _repo_path: &Path) -> usize {
        0
    }
}

struct UnusedXflow;

#[async_trait]
impl CrossRepoGraphProvider for UnusedXflow {
    async fn graph_for(&self, _repo: &ResolvedRepo) -> Result<Arc<CrossRepoGraph>, AppError> {
        Err(AppError::Unavailable {
            dependency: "cross-repo graph",
            message: "not used in doc_pack tests".into(),
            retryable: false,
        })
    }
}

/// Provider over one shared store whose registry entry can be swapped between
/// calls (publication-token tests) — `resolve_repo` reads the CURRENT entry.
struct SwappableProvider {
    entry: Mutex<RegistryEntry>,
    store: Arc<FakeStore>,
    /// Extra entries served on successive `resolve_repo` calls (front first);
    /// when empty, the current entry is served.
    resolve_repo_overrides: Mutex<Vec<RegistryEntry>>,
}

impl SwappableProvider {
    fn entry(&self) -> RegistryEntry {
        self.entry.lock().unwrap().clone()
    }
}

#[async_trait]
impl RepoContextProvider for SwappableProvider {
    fn catalog_snapshot(&self) -> RepoCatalogSnapshot {
        RepoCatalogSnapshot::for_test(
            "doc-test".into(),
            cih_core::Registry::default(),
            cih_core::GroupRegistry::default(),
        )
    }

    fn resolve_repo(&self, _selector: RepoSelector) -> Result<ResolvedRepo, AppError> {
        let mut overrides = self.resolve_repo_overrides.lock().unwrap();
        let entry = if overrides.is_empty() {
            self.entry()
        } else {
            overrides.remove(0)
        };
        Ok(ResolvedRepo::from_entry(entry))
    }

    async fn resolve(&self, _selector: RepoSelector) -> Result<Arc<RepoContext>, AppError> {
        Ok(Arc::new(RepoContext {
            repo: ResolvedRepo::from_entry(self.entry()),
            store: self.store.clone(),
            search: Arc::new(EmptySearch),
        }))
    }
}

fn registry_entry(repo_path: &str) -> RegistryEntry {
    RegistryEntry {
        repository_id: None,
        name: "doc-test".to_string(),
        path: repo_path.to_string(),
        graph_key: "doc-test".to_string(),
        artifacts_dir: String::new(),
        latest_artifact_version: None,
        published_artifact_version: None,
        published_graph_content_version: Some("v-abc".to_string()),
        published_epoch: Some("epoch-1".to_string()),
        community_artifacts_dir: None,
        indexed_at: "2026-01-01T00:00:00Z".to_string(),
        last_git_head: None,
        stats: RegistryStats::default(),
    }
}

struct Harness {
    service: DocPackService,
    store: Arc<FakeStore>,
    provider: Arc<SwappableProvider>,
    repo_dir: std::path::PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.repo_dir);
    }
}

/// A populated harness: a Route → handler flow with a caller, tests, DB
/// effects, and a real on-disk source file (the temp dir is the repo root).
fn harness(tag: &str) -> Harness {
    let repo_dir = std::env::temp_dir().join(format!(
        "cih-doc-pack-{tag}-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("t").len()
    ));
    let _ = std::fs::remove_dir_all(&repo_dir);
    std::fs::create_dir_all(repo_dir.join("src")).unwrap();
    std::fs::write(
        repo_dir.join(SOURCE_FILE),
        "// header\nfunction getThings() {\n  return db.query('SELECT 1');\n}\nmodule.exports = getThings;\n",
    )
    .unwrap();

    let store = Arc::new(FakeStore::default());
    let mut route = node(ROUTE_ID, NodeKind::Route, "GET /api/things", SOURCE_FILE);
    route.props = Some(serde_json::json!({"httpMethod": "GET", "path": "/api/things"}));
    store.insert_node(route);
    let mut handler = node(HANDLER_ID, NodeKind::Method, "getThings", SOURCE_FILE);
    handler.props = Some(serde_json::json!({
        "cyclomatic": 7, "cognitive": 9, "transitiveLoopDepth": 2, "isRecursive": false,
        "stereotype": "handler",
    }));
    store.insert_node(handler);
    store.insert_node(node(CALLER_ID, NodeKind::Method, "page", SOURCE_FILE));
    store.insert_node(node(CLASS_ID, NodeKind::Class, "Bar", SOURCE_FILE));
    store.insert_node(node(MEMBER_ID, NodeKind::Method, "save", SOURCE_FILE));
    store.insert_node(node(FIELD_ID, NodeKind::Field, "state", SOURCE_FILE));

    store.set_flow(
        ROUTE_ID,
        FlowPage {
            hops: vec![
                flow_hop(ROUTE_ID, NodeKind::Route, 0, None, None),
                flow_hop(
                    HANDLER_ID,
                    NodeKind::Method,
                    1,
                    Some(ROUTE_ID),
                    Some("HANDLES_ROUTE"),
                ),
            ],
            has_more: false,
            traversal: TraversalStats::default(),
        },
    );
    store.set_flow(
        HANDLER_ID,
        FlowPage {
            hops: vec![flow_hop(HANDLER_ID, NodeKind::Method, 0, None, None)],
            has_more: false,
            traversal: TraversalStats::default(),
        },
    );
    *store.db_effects.lock().unwrap() = vec![DbEffect {
        method: NodeId::new(HANDLER_ID),
        query: NodeId::new("DbQuery:com.acme.Api#SELECT_1"),
        operation: "SELECT".into(),
        table: "THINGS".into(),
        access: "READ".into(),
        sql_preview: "SELECT 1".into(),
    }];
    store.set_callers(
        ROUTE_ID,
        vec![node(CALLER_ID, NodeKind::Method, "page", SOURCE_FILE)],
    );
    *store.processes.lock().unwrap() = vec!["Process:com.acme.OrderFlow".to_string()];
    store.set_tests(
        ROUTE_ID,
        TestCoveragePage {
            tests: vec![node(
                "Method:com.acme.ApiTest#testRoute/0",
                NodeKind::Method,
                "testRoute",
                "src/api.test.js",
            )],
            has_more: false,
        },
    );
    store.set_tests(
        CLASS_ID,
        TestCoveragePage {
            tests: vec![node(
                MEMBER_TEST_ID,
                NodeKind::Method,
                "testSave",
                "src/bar.test.js",
            )],
            has_more: false,
        },
    );

    let provider = Arc::new(SwappableProvider {
        entry: Mutex::new(registry_entry(&repo_dir.display().to_string())),
        store: store.clone(),
        resolve_repo_overrides: Mutex::new(Vec::new()),
    });
    let repos = RepoContextService::new(provider.clone());
    let service = DocPackService::new(
        repos.clone(),
        GraphQueryService::new(
            repos.clone(),
            ChangeDetectionService::new(Arc::new(EmptyChangedFiles)),
        ),
        TestingService::new(repos.clone(), TaintService::new(Arc::new(UnusedArtifacts))),
        FileService::new(
            repos.clone(),
            ReadFileLimits {
                max_bytes: 1 << 20,
                max_lines: 2000,
            },
            Arc::new(GrepRuntime::for_tests()),
        ),
        ContractService::new(
            provider.clone(),
            Arc::new(UnusedXflow),
            Arc::new(UnusedArtifacts),
        ),
    );
    Harness {
        service,
        store,
        provider,
        repo_dir,
    }
}

fn pack_command(name: &str) -> DocPackCommand {
    DocPackCommand::try_new(name.into(), String::new(), String::new(), true, None).unwrap()
}

async fn resolved_pack(harness: &Harness, name: &str) -> DocPackOutput {
    match harness.service.execute(pack_command(name)).await.unwrap() {
        SymbolQueryOutput::Resolved(pack) => pack,
        SymbolQueryOutput::Ambiguous(_) => panic!("unexpected ambiguity for {name}"),
    }
}

// ---- command validation -----------------------------------------------------

#[test]
fn absent_sections_select_all_five_in_declaration_order() {
    let command = pack_command("X");
    assert_eq!(command.profile().sections, DocSection::ALL.to_vec());
}

#[test]
fn explicit_empty_sections_are_rejected() {
    let error =
        DocPackCommand::try_new("X".into(), String::new(), String::new(), true, Some(vec![]))
            .unwrap_err();
    assert!(matches!(
        error,
        AppError::InvalidInput {
            field: "sections",
            ..
        }
    ));
}

#[test]
fn unsafe_group_names_are_rejected_before_contract_io() {
    for group in ["..", "../shop", "/tmp/shop", "shop/team", "shop\\team"] {
        let error = DocPackCommand::try_new("X".into(), String::new(), group.into(), true, None)
            .unwrap_err();
        assert!(matches!(
            error,
            AppError::InvalidInput { field: "group", .. }
        ));
    }
}

#[test]
fn sections_deduplicate_and_normalize_to_declaration_order() {
    let command = DocPackCommand::try_new(
        "X".into(),
        String::new(),
        String::new(),
        true,
        Some(vec!["tests".into(), "flow".into(), "tests".into()]),
    )
    .unwrap();
    assert_eq!(
        command.profile().sections,
        vec![DocSection::Flow, DocSection::Tests]
    );
}

#[test]
fn unknown_section_is_rejected_naming_the_valid_set() {
    let error = DocPackCommand::try_new(
        "X".into(),
        String::new(),
        String::new(),
        true,
        Some(vec!["sauce".into()]),
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("flow, upstream, tests, source, contracts"));
}

#[test]
fn include_source_false_removes_source_and_rejects_empty_explicit_selection() {
    let command =
        DocPackCommand::try_new("X".into(), String::new(), String::new(), false, None).unwrap();
    assert!(!command.profile().sections.contains(&DocSection::Source));

    let error = DocPackCommand::try_new(
        "X".into(),
        String::new(),
        String::new(),
        false,
        Some(vec!["source".into()]),
    )
    .unwrap_err();
    assert!(error.to_string().contains("include_source=false"));
}

#[test]
fn doc_status_command_defaults_and_clamps() {
    let default = DocStatusCommand::try_new(String::new(), String::new(), 0).unwrap();
    assert_eq!(default.docs_dir, "docs");
    assert_eq!(default.max_pages, 100);
    let clamped = DocStatusCommand::try_new(String::new(), "docs".into(), 9_999).unwrap();
    assert_eq!(clamped.max_pages, 500);
    assert!(DocStatusCommand::try_new(String::new(), "../out".into(), 0).is_err());
    assert!(DocStatusCommand::try_new(String::new(), "/abs".into(), 0).is_err());
}

#[test]
fn profile_parse_accepts_empty_effective_sections_and_rejects_drift() {
    let empty = EvidenceProfileV1::parse(
        r#"{"schema":1,"group":null,"include_source":true,"sections":[]}"#,
    )
    .unwrap();
    assert!(empty.sections.is_empty());
    assert!(EvidenceProfileV1::parse(
        r#"{"schema":2,"group":null,"include_source":true,"sections":[]}"#
    )
    .is_err());
    assert!(EvidenceProfileV1::parse(
        r#"{"schema":1,"group":null,"include_source":false,"sections":["source"]}"#
    )
    .is_err());
    assert!(EvidenceProfileV1::parse("not json").is_err());
}

// ---- pack behavior ----------------------------------------------------------

#[tokio::test]
async fn route_pack_delivers_bounded_sections_profile_hash_and_markdown() {
    let harness = harness("route-pack");
    let pack = resolved_pack(&harness, ROUTE_ID).await;

    assert_eq!(pack.node_id, ROUTE_ID);
    assert_eq!(pack.repo, "doc-test");
    assert_eq!(pack.profile, pack.requested_profile);
    assert_eq!(pack.evidence_hash.len(), 32);
    assert_eq!(pack.graph_version.as_deref(), Some("v-abc"));
    assert_eq!(pack.identity.http_method.as_deref(), Some("GET"));

    let flow = match pack.flow.as_ref().expect("flow requested") {
        Section::Available { body, .. } => body,
        Section::Unavailable { reason, .. } => panic!("flow unavailable: {reason}"),
    };
    assert_eq!(flow.steps.len(), 2);
    assert_eq!(flow.db_effects.len(), 1);
    // Contracts were requested but no group was passed.
    match pack.contracts.as_ref().expect("contracts requested") {
        Section::Unavailable { reason, .. } => assert!(reason.contains("group")),
        Section::Available { .. } => panic!("contracts require a group"),
    }

    // Markdown: frontmatter, prose markers, mermaid fence, source fence.
    let markdown = &pack.markdown;
    assert!(markdown.starts_with("---\n"));
    assert!(markdown.contains(&format!("cih_evidence_hash: {}", pack.evidence_hash)));
    assert!(markdown.contains("cih_graph_version: \"v-abc\""));
    assert!(markdown.contains("cih_generator: doc_pack-v1"));
    assert!(markdown.contains("cih_profile: {\"schema\":1"));
    assert!(markdown.contains("cih_requested_profile: {\"schema\":1"));
    for marker in [
        "<!-- cih:prose:overview:start -->",
        "<!-- cih:prose:overview:end -->",
        "<!-- cih:prose:flow:start -->",
        "<!-- cih:prose:flow:end -->",
        "<!-- cih:prose:notes:start -->",
        "<!-- cih:prose:notes:end -->",
    ] {
        assert!(markdown.contains(marker), "missing {marker}");
    }
    assert!(markdown.contains("```mermaid\n"));
    assert!(markdown.contains("## Data access"));
    assert!(markdown.contains("| READ | THINGS | SELECT |"));
    assert!(markdown.contains("## Source"));
    assert!(markdown.contains("function getThings()"));
}

#[tokio::test]
async fn sanitized_flow_clears_call_sites_only_when_via_present() {
    let harness = harness("sanitize");
    let pack = resolved_pack(&harness, ROUTE_ID).await;
    let flow = match pack.flow.as_ref().unwrap() {
        Section::Available { body, .. } => body,
        Section::Unavailable { reason, .. } => panic!("flow unavailable: {reason}"),
    };
    let root = &flow.steps[0];
    assert!(root.via.is_none(), "root keeps via: None");
    let hop = &flow.steps[1];
    let via = hop.via.as_ref().expect("non-root hop keeps its edge");
    assert_eq!(via.kind, "HANDLES_ROUTE");
    assert!(via.call_sites.is_empty(), "call-site args are cleared");
    assert_eq!(
        hop.node.parent_id.as_ref().map(|id| id.as_str()),
        Some(ROUTE_ID)
    );
}

#[tokio::test]
async fn short_ambiguous_name_returns_standard_ambiguous_result() {
    let harness = harness("ambiguous");
    // Two nodes named "save".
    harness.store.insert_node(node(
        "Method:com.acme.Other#save/2",
        NodeKind::Method,
        "save",
        SOURCE_FILE,
    ));
    let output = harness.service.execute(pack_command("save")).await.unwrap();
    let value = serde_json::to_value(&output).unwrap();
    assert_eq!(value["status"], "ambiguous");
    assert!(value["candidates"].as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn class_pack_flow_unavailable_tests_cover_members() {
    let harness = harness("class");
    let pack = resolved_pack(&harness, CLASS_ID).await;
    match pack.flow.as_ref().unwrap() {
        Section::Unavailable { reason, remedy, .. } => {
            assert!(reason.contains("type"));
            assert!(remedy.as_deref().unwrap_or("").contains("trace_flow"));
        }
        Section::Available { .. } => panic!("class flow must be unavailable"),
    }
    let tests = match pack.tests.as_ref().unwrap() {
        Section::Available { body, .. } => body,
        Section::Unavailable { reason, .. } => panic!("tests unavailable: {reason}"),
    };
    assert_eq!(tests.scope, TestScope::DirectAndMembers);
    assert_eq!(tests.tests.len(), 1);
    assert_eq!(tests.tests[0].id, MEMBER_TEST_ID);
    // A class with only a member-targeting test must never claim "no tests".
    assert!(!pack.markdown.contains("No tests target"));
    // Data access never renders beneath an unavailable flow.
    assert!(!pack.markdown.contains("## Data access"));
}

#[tokio::test]
async fn one_section_failure_degrades_only_that_section() {
    let harness = harness("degrade");
    harness.store.fail_context.store(true, Ordering::Relaxed);
    let pack = resolved_pack(&harness, ROUTE_ID).await;
    match pack.upstream.as_ref().unwrap() {
        Section::Unavailable { reason, .. } => assert!(reason.contains("upstream query failed")),
        Section::Available { .. } => panic!("upstream must degrade"),
    }
    assert_eq!(pack.identity.id, ROUTE_ID, "identity survives");
    assert!(matches!(
        pack.flow.as_ref().unwrap(),
        Section::Available { .. }
    ));
}

#[tokio::test]
async fn tests_over_fetch_reports_incomplete_and_never_serializes_probe_row() {
    let harness = harness("overfetch");
    let many: Vec<Node> = (0..TESTS_MAX + 5)
        .map(|index| {
            node(
                &format!("Method:com.acme.T#t{index:03}/0"),
                NodeKind::Method,
                &format!("t{index:03}"),
                "src/t.test.js",
            )
        })
        .collect();
    harness.store.set_tests(
        ROUTE_ID,
        TestCoveragePage {
            tests: many,
            has_more: false,
        },
    );
    let pack = resolved_pack(&harness, ROUTE_ID).await;
    let tests = match pack.tests.as_ref().unwrap() {
        Section::Available { body, .. } => body,
        Section::Unavailable { reason, .. } => panic!("tests unavailable: {reason}"),
    };
    assert_eq!(tests.tests.len(), TESTS_MAX);
    assert_eq!(tests.test_count, TESTS_MAX);
    assert!(!tests.completeness.complete);
}

#[tokio::test]
async fn unsupported_kind_fails_before_section_work() {
    let harness = harness("kind-gate");
    let error = harness
        .service
        .execute(pack_command(FIELD_ID))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Field"));
    assert_eq!(
        harness.store.context_calls.load(Ordering::Relaxed),
        0,
        "no section query may run for an unsupported kind"
    );
}

// ---- hash contract ----------------------------------------------------------

#[tokio::test]
async fn identical_evidence_yields_identical_hashes() {
    let harness = harness("hash-stable");
    let first = resolved_pack(&harness, ROUTE_ID).await;
    let second = resolved_pack(&harness, ROUTE_ID).await;
    assert_eq!(first.evidence_hash, second.evidence_hash);
    assert_eq!(
        first.markdown, second.markdown,
        "rendering is deterministic"
    );
}

#[tokio::test]
async fn node_local_changes_move_the_hash() {
    let harness = harness("hash-moves");
    let base = resolved_pack(&harness, ROUTE_ID).await.evidence_hash;

    // Caller change.
    harness.store.set_callers(
        ROUTE_ID,
        vec![node(
            "Method:com.acme.New#caller/0",
            NodeKind::Method,
            "newCaller",
            SOURCE_FILE,
        )],
    );
    let after_caller = resolved_pack(&harness, ROUTE_ID).await.evidence_hash;
    assert_ne!(base, after_caller);

    // DB-effect change.
    harness.store.db_effects.lock().unwrap()[0].table = "OTHER".into();
    let after_effect = resolved_pack(&harness, ROUTE_ID).await.evidence_hash;
    assert_ne!(after_caller, after_effect);

    // Source change within the excerpt.
    std::fs::write(
        harness.repo_dir.join(SOURCE_FILE),
        "// header\nfunction getThings() {\n  return db.query('SELECT 2');\n}\n",
    )
    .unwrap();
    let after_source = resolved_pack(&harness, ROUTE_ID).await.evidence_hash;
    assert_ne!(after_effect, after_source);
}

#[tokio::test]
async fn provenance_changes_do_not_move_the_hash() {
    let harness = harness("hash-provenance");
    let base = resolved_pack(&harness, ROUTE_ID).await.evidence_hash;
    {
        let mut entry = harness.provider.entry.lock().unwrap();
        entry.published_graph_content_version = Some("v-def".into());
        entry.published_epoch = Some("epoch-2".into());
        entry.indexed_at = "2026-02-02T00:00:00Z".into();
    }
    let bumped = resolved_pack(&harness, ROUTE_ID).await;
    assert_eq!(
        base, bumped.evidence_hash,
        "repo-wide clocks are not hash input"
    );
    assert_eq!(
        bumped.graph_version.as_deref(),
        Some("v-def"),
        "provenance still reported"
    );
}

#[tokio::test]
async fn profile_changes_move_the_hash() {
    let harness = harness("hash-profile");
    let all = resolved_pack(&harness, ROUTE_ID).await.evidence_hash;
    let without_source = match harness
        .service
        .execute(
            DocPackCommand::try_new(ROUTE_ID.into(), String::new(), String::new(), false, None)
                .unwrap(),
        )
        .await
        .unwrap()
    {
        SymbolQueryOutput::Resolved(pack) => pack.evidence_hash,
        SymbolQueryOutput::Ambiguous(_) => panic!("unexpected ambiguity"),
    };
    assert_ne!(all, without_source);
}

#[tokio::test]
async fn equivalent_numeric_representations_hash_identically() {
    let harness = harness("hash-numeric");
    let base = resolved_pack(&harness, HANDLER_ID).await;
    assert_eq!(base.identity.cyclomatic, Some(7));

    // Same values as float and numeric-string legacy forms.
    let mut handler = node(HANDLER_ID, NodeKind::Method, "getThings", SOURCE_FILE);
    handler.props = Some(serde_json::json!({
        "cyclomatic": 7.0, "cognitive": "9", "transitiveLoopDepth": 2, "isRecursive": false,
        "stereotype": "handler",
    }));
    harness.store.insert_node(handler);
    let normalized = resolved_pack(&harness, HANDLER_ID).await;
    assert_eq!(base.evidence_hash, normalized.evidence_hash);

    // Malformed values are omitted with a warning, not hashed raw.
    let mut handler = node(HANDLER_ID, NodeKind::Method, "getThings", SOURCE_FILE);
    handler.props = Some(serde_json::json!({
        "cyclomatic": -1, "cognitive": 9, "transitiveLoopDepth": 2, "isRecursive": false,
        "stereotype": "handler",
    }));
    harness.store.insert_node(handler);
    let malformed = resolved_pack(&harness, HANDLER_ID).await;
    assert_eq!(malformed.identity.cyclomatic, None);
    assert!(malformed
        .warnings
        .iter()
        .any(|warning| warning.contains("cyclomatic")));
}

// ---- byte backstop ----------------------------------------------------------

/// Oversized flow: enough long-named hops to overflow 64 KiB on their own.
fn install_fat_flow(store: &FakeStore, entry: &str) {
    let hops: Vec<FlowHop> = std::iter::once(flow_hop(entry, NodeKind::Route, 0, None, None))
        .chain((0..99).map(|index| {
            let id = format!("Method:com.acme.Fat#{}{index:02}/0", "x".repeat(900));
            flow_hop(&id, NodeKind::Method, 1, Some(entry), Some("CALLS"))
        }))
        .collect();
    store.set_flow(
        entry,
        FlowPage {
            hops,
            has_more: false,
            traversal: TraversalStats::default(),
        },
    );
}

#[tokio::test]
async fn byte_backstop_drops_sections_recomputes_hash_and_stays_fresh() {
    let harness = harness("backstop");
    install_fat_flow(&harness.store, ROUTE_ID);
    let pack = resolved_pack(&harness, ROUTE_ID).await;

    // The fat flow forces drops; requested profile is preserved verbatim.
    assert!(pack.profile.sections.len() < pack.requested_profile.sections.len());
    assert_eq!(pack.requested_profile.sections, DocSection::ALL.to_vec());
    assert!(!pack.profile.sections.contains(&DocSection::Source));
    assert!(pack.source.is_none(), "dropped sections leave the response");
    assert!(pack
        .warnings
        .iter()
        .any(|warning| warning.contains("dropped section 'source'")));
    assert!(!pack.markdown.contains("## Source"));
    let bytes = serde_json::to_vec(&pack).unwrap().len();
    assert!(
        bytes + 512 <= 64 * 1024,
        "final serialized pack plus margin fits the cap: {bytes}"
    );

    // A page written from the reduced profile stays fresh under doc_status.
    let docs = harness.repo_dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("route.md"), &pack.markdown).unwrap();
    let status = harness
        .service
        .status(DocStatusCommand::try_new(String::new(), String::new(), 0).unwrap())
        .await
        .unwrap();
    assert_eq!(status.pages.len(), 1);
    let page = &status.pages[0];
    assert_eq!(page.status, DocStatus::Fresh, "reason: {:?}", page.reason);
    assert_eq!(page.profile_reduced, Some(true));
}

// ---- doc_status -------------------------------------------------------------

#[tokio::test]
async fn doc_status_fresh_then_stale_on_node_local_change_only() {
    let harness = harness("status-cycle");
    let pack = resolved_pack(&harness, ROUTE_ID).await;
    let docs = harness.repo_dir.join("docs");
    std::fs::create_dir_all(docs.join("api")).unwrap();
    std::fs::write(docs.join("api/route.md"), &pack.markdown).unwrap();
    // Ordinary markdown and non-CIH frontmatter are ignored.
    std::fs::write(docs.join("readme.md"), "# plain\n").unwrap();
    std::fs::write(docs.join("fm.md"), "---\ntitle: x\n---\nbody\n").unwrap();

    let command = || DocStatusCommand::try_new(String::new(), String::new(), 0).unwrap();
    let fresh = harness.service.status(command()).await.unwrap();
    assert_eq!(fresh.pages.len(), 1, "non-CIH files are not rows");
    assert_eq!(fresh.pages[0].status, DocStatus::Fresh);
    assert_eq!(fresh.pages[0].profile_reduced, None);

    // An unrelated provenance change keeps the page fresh…
    harness
        .provider
        .entry
        .lock()
        .unwrap()
        .published_graph_content_version = Some("v-zzz".into());
    let still_fresh = harness.service.status(command()).await.unwrap();
    assert_eq!(still_fresh.pages[0].status, DocStatus::Fresh);

    // …while a node-local change stales it.
    harness.store.set_callers(
        ROUTE_ID,
        vec![node(
            "Method:com.acme.New#caller/0",
            NodeKind::Method,
            "newCaller",
            SOURCE_FILE,
        )],
    );
    let stale = harness.service.status(command()).await.unwrap();
    assert_eq!(stale.pages[0].status, DocStatus::Stale);
    assert_ne!(stale.pages[0].current_hash, stale.pages[0].stored_hash);
}

#[tokio::test]
async fn doc_status_missing_node_and_store_error_rows() {
    let harness = harness("status-errors");
    let pack = resolved_pack(&harness, ROUTE_ID).await;
    let docs = harness.repo_dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("route.md"), &pack.markdown).unwrap();
    std::fs::write(
        docs.join("gone.md"),
        pack.markdown
            .replace(ROUTE_ID, "Route:GET /api/deleted")
            .as_str(),
    )
    .unwrap();

    let status = harness
        .service
        .status(DocStatusCommand::try_new(String::new(), String::new(), 0).unwrap())
        .await
        .unwrap();
    let by_path: BTreeMap<&str, &DocStatusPage> = status
        .pages
        .iter()
        .map(|page| (page.path.as_str(), page))
        .collect();
    assert_eq!(by_path["docs/gone.md"].status, DocStatus::MissingNode);
    assert_eq!(by_path["docs/route.md"].status, DocStatus::Fresh);

    // A current store failure on a requested section is `error`, never a
    // freshness verdict — and one page's error fails no other page.
    harness.store.fail_context.store(true, Ordering::Relaxed);
    let degraded = harness
        .service
        .status(DocStatusCommand::try_new(String::new(), String::new(), 0).unwrap())
        .await
        .unwrap();
    let route_row = degraded
        .pages
        .iter()
        .find(|page| page.path == "docs/route.md")
        .unwrap();
    assert_eq!(route_row.status, DocStatus::Error);
    assert!(route_row
        .reason
        .as_deref()
        .unwrap_or("")
        .contains("upstream"));
}

#[tokio::test]
async fn doc_status_unparseable_variants_and_late_keys() {
    let harness = harness("status-parse");
    let docs = harness.repo_dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    // CIH keys buried after 20+ ordinary frontmatter lines are still found.
    let pack = resolved_pack(&harness, ROUTE_ID).await;
    let padding: String = (0..25).map(|i| format!("extra_{i}: value\n")).collect();
    let buried = pack
        .markdown
        .replacen("---\n", &format!("---\n{padding}"), 1);
    std::fs::write(docs.join("buried.md"), buried).unwrap();
    // Partial CIH metadata is unparseable, not ignored.
    std::fs::write(
        docs.join("partial.md"),
        "---\ncih_node: \"Route:GET /x\"\n---\nbody\n",
    )
    .unwrap();
    // Unclosed frontmatter with CIH keys is unparseable.
    std::fs::write(
        docs.join("unclosed.md"),
        "---\ncih_node: \"Route:GET /x\"\n",
    )
    .unwrap();
    // Over-16-KiB frontmatter without a closing delimiter is
    // frontmatter_too_large, not silently ignored.
    let huge = format!(
        "---\ncih_node: \"Route:GET /x\"\n{}\n",
        "x".repeat(17 * 1024)
    );
    std::fs::write(docs.join("huge.md"), huge).unwrap();

    let status = harness
        .service
        .status(DocStatusCommand::try_new(String::new(), String::new(), 0).unwrap())
        .await
        .unwrap();
    let by_path: BTreeMap<&str, &DocStatusPage> = status
        .pages
        .iter()
        .map(|page| (page.path.as_str(), page))
        .collect();
    assert_eq!(by_path["docs/buried.md"].status, DocStatus::Fresh);
    assert_eq!(by_path["docs/partial.md"].status, DocStatus::Unparseable);
    assert_eq!(by_path["docs/unclosed.md"].status, DocStatus::Unparseable);
    assert_eq!(by_path["docs/huge.md"].status, DocStatus::Unparseable);
    assert_eq!(
        by_path["docs/huge.md"].reason.as_deref(),
        Some("frontmatter_too_large")
    );
    // Deterministic path ordering.
    let paths: Vec<&str> = status.pages.iter().map(|page| page.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted);
}

#[tokio::test]
async fn doc_status_caps_pages_and_walk_entries_and_skips_symlinks() {
    let harness = harness("status-caps");
    let pack = resolved_pack(&harness, ROUTE_ID).await;
    let docs = harness.repo_dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.md"), &pack.markdown).unwrap();
    std::fs::write(docs.join("b.md"), &pack.markdown).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(docs.join("a.md"), docs.join("link.md")).unwrap();
    // A file nested beyond the depth cap is never visited.
    let deep = docs.join("d1/d2/d3/d4");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("deep.md"), &pack.markdown).unwrap();

    let scan = scan_docs(&harness.repo_dir, "docs", 100).unwrap();
    assert_eq!(scan.candidates.len(), 2, "symlink + too-deep page skipped");

    let capped = scan_docs(&harness.repo_dir, "docs", 1).unwrap();
    assert_eq!(capped.candidates.len(), 1);
    assert!(capped.pages_capped);

    let entry_capped = scan_docs_with_caps(
        &harness.repo_dir,
        "docs",
        100,
        WalkCaps {
            max_depth: DOC_STATUS_WALK_MAX_DEPTH,
            max_entries: 1,
        },
    )
    .unwrap();
    assert!(entry_capped.entries_capped);

    // The service surfaces the caps as incompleteness.
    let status = harness
        .service
        .status(DocStatusCommand::try_new(String::new(), String::new(), 1).unwrap())
        .await
        .unwrap();
    assert!(!status.completeness.complete);
    assert!(status.completeness.reasons.contains(&"page_limit"));
}

#[test]
fn doc_status_scan_io_errors_are_not_silently_skipped() {
    let root = std::env::temp_dir().join(format!("cih-doc-status-io-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let docs = root.join("docs");
    std::fs::create_dir_all(&docs).unwrap();

    let missing_dir = docs.join("missing");
    let read_error = read_scan_entries(&root, &missing_dir).unwrap_err();
    assert!(matches!(
        read_error,
        AppError::Unavailable {
            dependency: "doc_status scan",
            retryable: true,
            ..
        }
    ));

    let missing_page = docs.join("missing.md");
    let metadata_error = scan_entry_metadata(&root, &missing_page).unwrap_err();
    assert!(matches!(
        metadata_error,
        AppError::Unavailable {
            dependency: "doc_status scan",
            retryable: true,
            ..
        }
    ));
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn doc_status_missing_docs_dir_is_explicitly_incomplete() {
    let harness = harness("status-missing-docs");
    let status = harness
        .service
        .status(DocStatusCommand::try_new(String::new(), "missing-docs".into(), 0).unwrap())
        .await
        .unwrap();
    assert!(status.pages.is_empty());
    assert!(!status.completeness.complete);
    assert!(status.completeness.reasons.contains(&"docs_dir_missing"));
}

#[tokio::test]
async fn doc_status_deduplicates_identical_node_profile_pairs() {
    let harness = harness("status-dedupe");
    let pack = resolved_pack(&harness, ROUTE_ID).await;
    let docs = harness.repo_dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("one.md"), &pack.markdown).unwrap();
    std::fs::write(docs.join("two.md"), &pack.markdown).unwrap();

    let before = harness.store.context_calls.load(Ordering::Relaxed);
    let status = harness
        .service
        .status(DocStatusCommand::try_new(String::new(), String::new(), 0).unwrap())
        .await
        .unwrap();
    assert_eq!(status.pages.len(), 2);
    assert!(status
        .pages
        .iter()
        .all(|page| page.status == DocStatus::Fresh));
    let rebuild_context_calls = harness.store.context_calls.load(Ordering::Relaxed) - before;
    assert_eq!(
        rebuild_context_calls, 1,
        "two identical pages must trigger exactly one evidence rebuild"
    );
}

#[tokio::test]
async fn doc_status_aborts_when_publication_changes_mid_batch() {
    let harness = harness("status-token");
    let pack = resolved_pack(&harness, ROUTE_ID).await;
    let docs = harness.repo_dir.join("docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("route.md"), &pack.markdown).unwrap();

    let mut changed = harness.provider.entry();
    changed.indexed_at = "2026-03-03T00:00:00Z".into();
    harness
        .provider
        .resolve_repo_overrides
        .lock()
        .unwrap()
        .push(changed);
    let error = harness
        .service
        .status(DocStatusCommand::try_new(String::new(), String::new(), 0).unwrap())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("publication changed"));
}

#[tokio::test]
async fn doc_pack_retries_once_on_publication_change_then_errors() {
    let harness = harness("pack-token");
    // First re-check sees a changed token; the retry sees a stable one.
    let mut changed = harness.provider.entry();
    changed.indexed_at = "2026-03-03T00:00:00Z".into();
    {
        let mut overrides = harness.provider.resolve_repo_overrides.lock().unwrap();
        overrides.push(changed.clone());
    }
    // After the override is consumed, resolve() still serves the ORIGINAL
    // entry, so the retry's before/after tokens agree.
    let pack = resolved_pack(&harness, ROUTE_ID).await;
    assert_eq!(pack.node_id, ROUTE_ID);

    // Two consecutive mismatching re-checks exhaust the retry.
    {
        let mut overrides = harness.provider.resolve_repo_overrides.lock().unwrap();
        let mut second = harness.provider.entry();
        second.indexed_at = "2026-04-04T00:00:00Z".into();
        let mut third = harness.provider.entry();
        third.indexed_at = "2026-05-05T00:00:00Z".into();
        overrides.push(second);
        overrides.push(third);
    }
    let error = harness
        .service
        .execute(pack_command(ROUTE_ID))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("changed twice"));
}

// ---- snapshot token ---------------------------------------------------------

#[test]
fn snapshot_token_equality_covers_published_and_unpublished_modes() {
    let published = registry_entry("/tmp/x");
    assert_eq!(
        RepoSnapshotToken::from_entry(&published),
        RepoSnapshotToken::from_entry(&published.clone())
    );
    let mut unpublished = registry_entry("/tmp/x");
    unpublished.published_epoch = None;
    unpublished.published_graph_content_version = None;
    let same = RepoSnapshotToken::from_entry(&unpublished);
    assert_eq!(same, RepoSnapshotToken::from_entry(&unpublished.clone()));
    // In unpublished mode, `indexed_at` alone trips the guard.
    let mut reindexed = unpublished.clone();
    reindexed.indexed_at = "2027-01-01T00:00:00Z".into();
    assert_ne!(same, RepoSnapshotToken::from_entry(&reindexed));
}

// ---- renderer ---------------------------------------------------------------

#[tokio::test]
async fn renderer_escapes_frontmatter_and_adapts_fences() {
    let harness = harness("render-escape");
    // A node name with quotes/backticks and source content with a fence run.
    let mut tricky = node(
        "Method:com.acme.Tricky#run/0",
        NodeKind::Method,
        "tri\"cky",
        SOURCE_FILE,
    );
    tricky.qualified_name = Some("com.acme.Tri\"cky#run".to_string());
    harness.store.insert_node(tricky);
    harness.store.set_flow(
        "Method:com.acme.Tricky#run/0",
        FlowPage {
            hops: vec![flow_hop(
                "Method:com.acme.Tricky#run/0",
                NodeKind::Method,
                0,
                None,
                None,
            )],
            has_more: false,
            traversal: TraversalStats::default(),
        },
    );
    std::fs::write(
        harness.repo_dir.join(SOURCE_FILE),
        "line1\n```mermaid\nembedded fence\n```\nline5\n",
    )
    .unwrap();
    let pack = resolved_pack(&harness, "Method:com.acme.Tricky#run/0").await;
    assert!(pack.markdown.contains(r#"title: "com.acme.Tri\"cky#run""#));
    // The source fence must be longer than the embedded three-backtick run.
    assert!(pack.markdown.contains("````js\n"));
}

#[tokio::test]
async fn renderer_omits_graph_version_when_unpublished() {
    let harness = harness("render-unpublished");
    {
        let mut entry = harness.provider.entry.lock().unwrap();
        entry.published_graph_content_version = None;
        entry.published_epoch = None;
    }
    let pack = resolved_pack(&harness, ROUTE_ID).await;
    assert!(pack.graph_version.is_none());
    assert!(!pack.markdown.contains("cih_graph_version"));
}

#[tokio::test]
async fn renderer_uses_scope_aware_no_tests_wording() {
    let harness = harness("render-wording");
    harness.store.set_tests(
        ROUTE_ID,
        TestCoveragePage {
            tests: Vec::new(),
            has_more: false,
        },
    );
    let route = resolved_pack(&harness, ROUTE_ID).await;
    assert!(route.markdown.contains("No tests target this symbol."));

    let member = resolved_pack(&harness, MEMBER_ID).await;
    assert!(member
        .markdown
        .contains("No tests target this callable or its owning type."));

    harness.store.set_tests(
        CLASS_ID,
        TestCoveragePage {
            tests: Vec::new(),
            has_more: false,
        },
    );
    let class = resolved_pack(&harness, CLASS_ID).await;
    assert!(class
        .markdown
        .contains("No tests target this type or its indexed members."));

    // An incomplete empty result is inconclusive, never a "none" claim.
    harness.store.set_tests(
        ROUTE_ID,
        TestCoveragePage {
            tests: Vec::new(),
            has_more: true,
        },
    );
    let inconclusive = resolved_pack(&harness, ROUTE_ID).await;
    assert!(inconclusive.markdown.contains("inconclusive"));
    assert!(!inconclusive.markdown.contains("No tests target"));
}

// ---- prose-preserving regeneration fixture ----------------------------------

/// The documented client-side algorithm: extract marker-owned prose, refuse
/// structurally ambiguous pages, splice into a fresh skeleton.
fn extract_prose(page: &str, name: &str) -> Result<String, String> {
    let start = format!("<!-- cih:prose:{name}:start -->");
    let end = format!("<!-- cih:prose:{name}:end -->");
    if page.matches(&start).count() != 1 || page.matches(&end).count() != 1 {
        return Err(format!("markers for '{name}' are missing or duplicated"));
    }
    let after = page.split(&start).nth(1).unwrap();
    let Some(body) = after.split(&end).next() else {
        return Err(format!("markers for '{name}' are out of order"));
    };
    Ok(body.to_string())
}

fn splice_prose(skeleton: &str, name: &str, prose: &str) -> String {
    let start = format!("<!-- cih:prose:{name}:start -->");
    let end = format!("<!-- cih:prose:{name}:end -->");
    skeleton.replace(&format!("{start}\n{end}"), &format!("{start}{prose}{end}"))
}

#[tokio::test]
async fn prose_blocks_survive_regeneration_and_ambiguity_is_refused() {
    let harness = harness("prose");
    let pack = resolved_pack(&harness, ROUTE_ID).await;

    // An agent writes prose into the three marker blocks.
    let mut page = pack.markdown.clone();
    for (name, prose) in [
        ("overview", "\nThis endpoint lists things.\n"),
        ("flow", "\nThe handler queries THINGS.\n"),
        ("notes", "\nRate-limited upstream.\n"),
    ] {
        page = splice_prose(&page, name, prose);
    }

    // Evidence changes; the skeleton is regenerated; prose is spliced back.
    harness.store.set_callers(
        ROUTE_ID,
        vec![node(
            "Method:com.acme.New#caller/0",
            NodeKind::Method,
            "newCaller",
            SOURCE_FILE,
        )],
    );
    let regenerated = resolved_pack(&harness, ROUTE_ID).await;
    assert_ne!(regenerated.evidence_hash, pack.evidence_hash);
    let mut fresh_page = regenerated.markdown.clone();
    for name in ["overview", "flow", "notes"] {
        let prose = extract_prose(&page, name).unwrap();
        fresh_page = splice_prose(&fresh_page, name, &prose);
    }
    assert!(fresh_page.contains("This endpoint lists things."));
    assert!(fresh_page.contains("The handler queries THINGS."));
    assert!(fresh_page.contains("Rate-limited upstream."));
    assert!(fresh_page.contains(&regenerated.evidence_hash));

    // Duplicated markers make extraction refuse rather than guess.
    let duplicated = format!("{page}\n<!-- cih:prose:notes:start -->\n");
    assert!(extract_prose(&duplicated, "notes").is_err());
    // Missing markers likewise.
    assert!(extract_prose("# no markers", "overview").is_err());
}
