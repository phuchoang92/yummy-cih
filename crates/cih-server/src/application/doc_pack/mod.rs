//! `doc_pack` + `doc_status` — per-node documentation evidence packs.
//!
//! `doc_pack` returns a curated, bounded evidence pack for one node (identity +
//! flow + upstream + tests + source + cross-repo consumers) plus a
//! deterministic markdown skeleton, an [`EvidenceProfileV1`] describing exactly
//! which evidence was delivered, and a blake3 `evidence_hash` over that
//! node-local evidence. `doc_status` re-derives the same hash for every
//! generated page under a docs directory and reports fresh/stale per page.
//!
//! Invariants (design record: `docs/plans/doc-pack-and-doc-status.md`):
//! per-node staleness (no repo-wide clock is hash input), a reproducible
//! serialized profile, one version-bound repository context per build, bounded
//! queries before rendering, hash-what-was-delivered under the byte backstop,
//! and honest per-section degradation.

mod render;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cih_core::{Node, NodeId, NodeKind};
use cih_graph_store::{ContextFilter, DbEffect, FlowHop};
use serde::{Deserialize, Serialize};

use crate::application::app_services::RepoContextService;
use crate::application::contracts::{ContractService, RouteConsumer, RouteConsumersQuery};
use crate::application::files::{FileService, SourceSpan, SourceSpanCommand};
use crate::application::graph::{
    resolve_symbol, AmbiguousResult, GraphQueryService, SymbolQueryOutput, SymbolResolution,
    TraceFlowCommand,
};
use crate::application::section::Section;
use crate::application::testing::TestingService;
use crate::domain::completeness::ResultBounds;
use crate::domain::error::AppError;
use crate::domain::repository::RepoSelector;
use crate::ports::blocking_runtime::{blocking_timeout, run_blocking};
use crate::ports::repo_context_provider::RepoContext;

pub(crate) use render::render_doc_page;

// ---- hard bounds (applied before fingerprinting/rendering) -----------------

const FLOW_MAX_DEPTH: u32 = 6;
const FLOW_MAX_NODES: usize = 100;
const UPSTREAM_MAX_CALLERS: usize = 50;
const UPSTREAM_MAX_PROCESSES: usize = 25;
const TESTS_MAX: usize = 50;
const SOURCE_MAX_LINES: usize = 120;
const SOURCE_MAX_BYTES: usize = 8 * 1024;
const CONTRACTS_MAX_CONSUMERS: usize = 50;

/// Response self-cap: double architecture-overview's 32 KiB because a pack
/// carries a source excerpt plus a Mermaid-backed markdown rendering; still
/// far below the transport's 256 KiB soft target.
const DOC_PACK_MAX_RESPONSE_BYTES: usize = 64 * 1024;
const BACKSTOP_MARGIN_BYTES: usize = 512;

const DOC_STATUS_DEFAULT_MAX_PAGES: usize = 100;
const DOC_STATUS_MAX_PAGES: usize = 500;
const DOC_STATUS_WALK_MAX_DEPTH: usize = 4;
const DOC_STATUS_WALK_MAX_ENTRIES: usize = 10_000;
const DOC_STATUS_FRONTMATTER_MAX_BYTES: usize = 16 * 1024;
const DOC_STATUS_REBUILD_CONCURRENCY: usize = 4;

pub(crate) const DOC_GENERATOR: &str = "doc_pack-v1";

// ---- sections and profile ---------------------------------------------------

/// Selectable evidence sections, in canonical declaration order. Identity is
/// mandatory and not selectable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DocSection {
    Flow,
    Upstream,
    Tests,
    Source,
    Contracts,
}

impl DocSection {
    const ALL: [DocSection; 5] = [
        DocSection::Flow,
        DocSection::Upstream,
        DocSection::Tests,
        DocSection::Source,
        DocSection::Contracts,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            DocSection::Flow => "flow",
            DocSection::Upstream => "upstream",
            DocSection::Tests => "tests",
            DocSection::Source => "source",
            DocSection::Contracts => "contracts",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == raw)
    }

    /// The standalone tool that re-fetches this section when the byte
    /// backstop drops it (named in the drop warning).
    fn refetch_tool(self) -> &'static str {
        match self {
            DocSection::Flow => "trace_flow",
            DocSection::Upstream => "context",
            DocSection::Tests => "test_coverage",
            DocSection::Source => "read_file",
            DocSection::Contracts => "api_impact",
        }
    }
}

/// Byte-backstop drop order: first entry dropped first. Identity is never
/// dropped.
const DROP_ORDER: [DocSection; 5] = [
    DocSection::Source,
    DocSection::Contracts,
    DocSection::Flow,
    DocSection::Upstream,
    DocSection::Tests,
];

/// The versioned, reproducible description of exactly which evidence a pack
/// delivers. Serialized verbatim into JSON and frontmatter; the **effective**
/// profile is hash input, the requested profile is regeneration metadata.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct EvidenceProfileV1 {
    /// Always 1.
    pub(crate) schema: u8,
    pub(crate) group: Option<String>,
    pub(crate) include_source: bool,
    /// Normalized (declaration-ordered, deduplicated) effective sections.
    pub(crate) sections: Vec<DocSection>,
}

impl EvidenceProfileV1 {
    /// Parse a frontmatter profile value. An empty **effective** section list
    /// is legal (the byte backstop may reduce a pack to identity only);
    /// callers that parse `cih_requested_profile` additionally require
    /// non-empty sections.
    fn parse(raw: &str) -> Result<Self, String> {
        let mut profile: EvidenceProfileV1 =
            serde_json::from_str(raw).map_err(|e| format!("invalid profile JSON: {e}"))?;
        if profile.schema != 1 {
            return Err(format!("unsupported profile schema {}", profile.schema));
        }
        profile.sections.sort();
        profile.sections.dedup();
        if !profile.include_source && profile.sections.contains(&DocSection::Source) {
            return Err("profile lists 'source' while include_source is false".into());
        }
        Ok(profile)
    }
}

// ---- command ---------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct DocPackCommand {
    repo: RepoSelector,
    name: String,
    group: Option<String>,
    include_source: bool,
    /// Normalized effective sections (declaration order, deduplicated,
    /// `source` removed when `include_source` is false).
    sections: Vec<DocSection>,
}

impl DocPackCommand {
    pub(crate) fn try_new(
        name: String,
        repo: String,
        group: String,
        include_source: bool,
        sections: Option<Vec<String>>,
    ) -> Result<Self, AppError> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(AppError::InvalidInput {
                field: "name",
                message: "must not be empty".into(),
            });
        }
        let group = {
            let group = group.trim();
            (!group.is_empty()).then(|| group.to_string())
        };
        let explicit = sections.is_some();
        let mut selected = match sections {
            None => DocSection::ALL.to_vec(),
            Some(raw) => {
                if raw.is_empty() {
                    return Err(AppError::InvalidInput {
                        field: "sections",
                        message: "an explicit empty section list is invalid; omit `sections` \
                                  to request every section"
                            .into(),
                    });
                }
                let mut parsed = Vec::with_capacity(raw.len());
                for value in &raw {
                    let value = value.trim();
                    let Some(section) = DocSection::parse(value) else {
                        return Err(AppError::InvalidInput {
                            field: "sections",
                            message: format!(
                                "unknown section '{value}'; valid sections: {}",
                                DocSection::ALL
                                    .iter()
                                    .map(|s| s.name())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        });
                    };
                    parsed.push(section);
                }
                parsed.sort();
                parsed.dedup();
                parsed
            }
        };
        if !include_source {
            selected.retain(|section| *section != DocSection::Source);
            if explicit && selected.is_empty() {
                return Err(AppError::InvalidInput {
                    field: "sections",
                    message: "include_source=false removes 'source', leaving no requested \
                              sections"
                        .into(),
                });
            }
        }
        Ok(Self {
            repo: RepoSelector::from_wire(&repo),
            name,
            group,
            include_source,
            sections: selected,
        })
    }

    fn profile(&self) -> EvidenceProfileV1 {
        EvidenceProfileV1 {
            schema: 1,
            group: self.group.clone(),
            include_source: self.include_source,
            sections: self.sections.clone(),
        }
    }
}

// ---- response bodies -------------------------------------------------------

/// Mandatory identity: node fields plus curated, **typed** props. Backed by
/// concrete fields (never a raw props map) so equivalent JSON number
/// representations cannot cause false staleness.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct IdentityBody {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) qualified_name: Option<String>,
    pub(crate) file: String,
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) http_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stereotype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cyclomatic: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cognitive: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) transitive_loop_depth: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) is_recursive: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SymbolRef {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) name: String,
    pub(crate) file: String,
}

impl SymbolRef {
    fn from_node(node: &Node) -> Self {
        Self {
            id: node.id.as_str().to_string(),
            kind: node.kind.label().to_string(),
            name: node.name.clone(),
            file: node.file.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FlowBody {
    pub(crate) steps: Vec<FlowHop>,
    pub(crate) db_effects: Vec<DbEffect>,
    pub(crate) completeness: ResultBounds,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct UpstreamBody {
    pub(crate) callers: Vec<SymbolRef>,
    pub(crate) processes: Vec<String>,
    pub(crate) completeness: ResultBounds,
}

/// Stable scope label for the tests section — which targets the bounded query
/// covered for this node kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TestScope {
    /// Direct TESTS edges only (Function, Route).
    Direct,
    /// Direct plus tests of the owning type (Method, Constructor).
    DirectAndOwner,
    /// Direct plus tests targeting indexed members (Class, Interface).
    DirectAndMembers,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TestsBody {
    pub(crate) scope: TestScope,
    /// The number of tests returned — not an asserted total when the section
    /// is incomplete.
    pub(crate) test_count: usize,
    pub(crate) tests: Vec<SymbolRef>,
    pub(crate) completeness: ResultBounds,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ContractsBody {
    pub(crate) consumers: Vec<RouteConsumer>,
    pub(crate) completeness: ResultBounds,
    /// Freshness provenance — never hash input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) contracts_synced_at: Option<String>,
    pub(crate) contracts_stale: bool,
}

/// The tool's full response. `None` sections were not requested or were
/// dropped by the byte backstop; `available: false` sections were requested
/// but are not available.
#[derive(Debug, Serialize)]
pub(crate) struct DocPackOutput {
    pub(crate) repo: String,
    pub(crate) node_id: String,
    /// The caller's normalized intent, before any byte-backstop drop —
    /// regeneration metadata, not hash input.
    pub(crate) requested_profile: EvidenceProfileV1,
    /// The delivered (effective) profile — hash input.
    pub(crate) profile: EvidenceProfileV1,
    pub(crate) evidence_hash: String,
    /// Diagnostics-only provenance, never staleness input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) graph_version: Option<String>,
    pub(crate) identity: IdentityBody,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) flow: Option<Section<FlowBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) upstream: Option<Section<UpstreamBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tests: Option<Section<TestsBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source: Option<Section<SourceSpan>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) contracts: Option<Section<ContractsBody>>,
    pub(crate) markdown: String,
    pub(crate) warnings: Vec<String>,
}

// ---- internal section state ------------------------------------------------

/// Stable machine-readable cause for a deliberately or transiently
/// unavailable section. Codes are hash input; human reason/remedy text never
/// is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnavailableCode {
    /// Contracts apply only to Route nodes.
    RoutesOnly,
    /// Flow starts at a callable; this node is a type.
    MemberRequired,
    /// Contracts need a `group` argument.
    GroupRequired,
    /// The Route node lacks `httpMethod`/`path` props.
    MissingRouteProps,
    /// A current backend/read failure — `doc_status` reports `error` instead
    /// of comparing a partial hash.
    RuntimeError,
}

/// One selectable section during a build: not requested (or dropped), served,
/// or requested-but-unavailable.
#[derive(Clone, Debug)]
pub(crate) enum SectionState<T> {
    Off,
    Ok {
        body: T,
    },
    Unavailable {
        code: UnavailableCode,
        reason: String,
        remedy: Option<String>,
    },
}

impl<T: Clone + Serialize> SectionState<T> {
    fn to_section(&self, source_label: &'static str) -> Option<Section<T>> {
        match self {
            SectionState::Off => None,
            SectionState::Ok { body } => Some(Section::ok(source_label, body.clone())),
            SectionState::Unavailable { reason, remedy, .. } => {
                Some(Section::off(reason.clone(), remedy.clone()))
            }
        }
    }

    fn fingerprint(&self) -> Option<FingerprintSection<&T>> {
        match self {
            SectionState::Off => None,
            SectionState::Ok { body } => Some(FingerprintSection::Available(body)),
            SectionState::Unavailable { code, .. } => {
                Some(FingerprintSection::Unavailable { code: *code })
            }
        }
    }
}

fn runtime_error_section<T>(what: &str, error: &AppError) -> SectionState<T> {
    SectionState::Unavailable {
        code: UnavailableCode::RuntimeError,
        reason: format!("{what} failed: {error}"),
        remedy: Some(
            "check the graph backend / server logs — this is a serving error, not a fact \
             about the codebase"
                .into(),
        ),
    }
}

/// Everything `doc_pack` and `doc_status` share: identity plus the
/// profile-selected section states, built from one version-bound repository
/// context.
pub(crate) struct EvidenceBundle {
    pub(crate) identity: IdentityBody,
    pub(crate) flow: SectionState<FlowBody>,
    pub(crate) upstream: SectionState<UpstreamBody>,
    pub(crate) tests: SectionState<TestsBody>,
    pub(crate) source: SectionState<SourceSpan>,
    pub(crate) contracts: SectionState<ContractsBody>,
    pub(crate) warnings: Vec<String>,
    /// True when any requested section failed on a current runtime/store
    /// error: `doc_status` must report `error`, never compare a partial hash.
    pub(crate) had_runtime_error: bool,
}

impl EvidenceBundle {
    fn drop_section(&mut self, section: DocSection) {
        match section {
            DocSection::Flow => self.flow = SectionState::Off,
            DocSection::Upstream => self.upstream = SectionState::Off,
            DocSection::Tests => self.tests = SectionState::Off,
            DocSection::Source => self.source = SectionState::Off,
            DocSection::Contracts => self.contracts = SectionState::Off,
        }
    }
}

// ---- fingerprint and hash ---------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum FingerprintSection<T: Serialize> {
    Available(T),
    Unavailable { code: UnavailableCode },
}

/// Contracts fingerprint: consumers + completeness only. The freshness stamps
/// (`contracts_synced_at`/`contracts_stale`) are call-time provenance.
#[derive(Serialize)]
struct ContractsFingerprint<'a> {
    consumers: &'a [RouteConsumer],
    completeness: &'a ResultBounds,
}

/// Exactly the node-local delivered evidence: schema tag + node id + effective
/// profile + identity + profile-selected section bodies. Excludes
/// graph_version, publication epoch, indexed time, contract-sync stamps,
/// warnings, remedies, and markdown.
#[derive(Serialize)]
struct EvidenceFingerprintV1<'a> {
    schema: &'static str,
    node_id: &'a str,
    profile: &'a EvidenceProfileV1,
    identity: &'a IdentityBody,
    #[serde(skip_serializing_if = "Option::is_none")]
    flow: Option<FingerprintSection<&'a FlowBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream: Option<FingerprintSection<&'a UpstreamBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tests: Option<FingerprintSection<&'a TestsBody>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<FingerprintSection<&'a SourceSpan>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contracts: Option<FingerprintSection<ContractsFingerprint<'a>>>,
}

const FINGERPRINT_SCHEMA: &str = "cih.doc_pack.evidence.v1";

fn build_fingerprint<'a>(
    node_id: &'a str,
    profile: &'a EvidenceProfileV1,
    bundle: &'a EvidenceBundle,
) -> EvidenceFingerprintV1<'a> {
    let wants = |section: DocSection| profile.sections.contains(&section);
    EvidenceFingerprintV1 {
        schema: FINGERPRINT_SCHEMA,
        node_id,
        profile,
        identity: &bundle.identity,
        flow: wants(DocSection::Flow)
            .then(|| bundle.flow.fingerprint())
            .flatten(),
        upstream: wants(DocSection::Upstream)
            .then(|| bundle.upstream.fingerprint())
            .flatten(),
        tests: wants(DocSection::Tests)
            .then(|| bundle.tests.fingerprint())
            .flatten(),
        source: wants(DocSection::Source)
            .then(|| bundle.source.fingerprint())
            .flatten(),
        contracts: wants(DocSection::Contracts)
            .then(|| match &bundle.contracts {
                SectionState::Off => None,
                SectionState::Ok { body } => {
                    Some(FingerprintSection::Available(ContractsFingerprint {
                        consumers: &body.consumers,
                        completeness: &body.completeness,
                    }))
                }
                SectionState::Unavailable { code, .. } => {
                    Some(FingerprintSection::Unavailable { code: *code })
                }
            })
            .flatten(),
    }
}

/// First 32 lowercase hex characters of blake3 over the canonical JSON
/// serialization of the fingerprint. Both tools call this one function.
fn evidence_hash(fingerprint: &EvidenceFingerprintV1<'_>) -> Result<String, AppError> {
    let bytes = serde_json::to_vec(fingerprint).map_err(|error| AppError::Unavailable {
        dependency: "evidence serialization",
        message: error.to_string(),
        retryable: false,
    })?;
    Ok(blake3::hash(&bytes).to_hex().as_str()[..32].to_string())
}

// ---- identity extraction ----------------------------------------------------

const SUPPORTED_KINDS: [NodeKind; 6] = [
    NodeKind::Route,
    NodeKind::Method,
    NodeKind::Function,
    NodeKind::Constructor,
    NodeKind::Class,
    NodeKind::Interface,
];

fn kind_supported(kind: NodeKind) -> bool {
    SUPPORTED_KINDS.contains(&kind)
}

fn prop_str(props: Option<&serde_json::Value>, key: &str) -> Option<String> {
    props?
        .get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Normalize equivalent integral JSON representations (u64, integral f64,
/// documented numeric-string legacy forms) to one canonical non-negative u64.
/// Malformed/out-of-range values become absent with a warning so backend
/// number formatting cannot cause false staleness.
fn prop_u64(
    props: Option<&serde_json::Value>,
    key: &str,
    warnings: &mut Vec<String>,
) -> Option<u64> {
    let value = props?.get(key)?;
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    if let Some(float) = value.as_f64() {
        if float >= 0.0 && float.fract() == 0.0 && float <= u64::MAX as f64 {
            return Some(float as u64);
        }
    }
    if let Some(text) = value.as_str() {
        if let Ok(number) = text.trim().parse::<u64>() {
            return Some(number);
        }
    }
    warnings.push(format!(
        "identity property '{key}' has a non-integral value ({value}) — omitted"
    ));
    None
}

fn prop_bool(
    props: Option<&serde_json::Value>,
    key: &str,
    warnings: &mut Vec<String>,
) -> Option<bool> {
    let value = props?.get(key)?;
    if let Some(flag) = value.as_bool() {
        return Some(flag);
    }
    warnings.push(format!(
        "identity property '{key}' has a non-boolean value ({value}) — omitted"
    ));
    None
}

fn identity_from_node(node: &Node, warnings: &mut Vec<String>) -> IdentityBody {
    let props = node.props.as_ref();
    IdentityBody {
        id: node.id.as_str().to_string(),
        kind: node.kind.label().to_string(),
        name: node.name.clone(),
        qualified_name: node.qualified_name.clone(),
        file: node.file.clone(),
        start_line: node.range.start_line,
        end_line: node.range.end_line,
        http_method: prop_str(props, "httpMethod"),
        path: prop_str(props, "path"),
        stereotype: prop_str(props, "stereotype"),
        cyclomatic: prop_u64(props, "cyclomatic", warnings),
        cognitive: prop_u64(props, "cognitive", warnings),
        transitive_loop_depth: prop_u64(props, "transitiveLoopDepth", warnings),
        is_recursive: prop_bool(props, "isRecursive", warnings),
    }
}

// ---- snapshot token ---------------------------------------------------------

/// Publication identity of the resolved repository at one instant. Both
/// published fields are legitimately `None` for repositories that never
/// recorded a publication; equality over the full tuple is the before/after
/// consistency check in that mode too.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RepoSnapshotToken {
    published_epoch: Option<String>,
    published_graph_content_version: Option<String>,
    indexed_at: String,
}

impl RepoSnapshotToken {
    fn from_entry(entry: &cih_core::RegistryEntry) -> Self {
        Self {
            published_epoch: entry.published_epoch.clone(),
            published_graph_content_version: entry.published_graph_content_version.clone(),
            indexed_at: entry.indexed_at.clone(),
        }
    }
}

// ---- service ----------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct DocPackService {
    repos: RepoContextService,
    graph: GraphQueryService,
    testing: TestingService,
    files: FileService,
    contracts: ContractService,
}

impl DocPackService {
    pub(crate) fn new(
        repos: RepoContextService,
        graph: GraphQueryService,
        testing: TestingService,
        files: FileService,
        contracts: ContractService,
    ) -> Self {
        Self {
            repos,
            graph,
            testing,
            files,
            contracts,
        }
    }

    pub(crate) async fn execute(
        &self,
        command: DocPackCommand,
    ) -> Result<SymbolQueryOutput<DocPackOutput>, AppError> {
        // One retry when the repository's publication changes mid-build; a
        // second change is a fault the caller should see.
        for last_attempt in [false, true] {
            let context = self.repos.resolve(command.repo.clone()).await?;
            let token = RepoSnapshotToken::from_entry(&context.repo.registry_entry);
            let outcome = self.build_pack(&context, &command).await?;
            let SymbolQueryOutput::Resolved(pack) = outcome else {
                return Ok(outcome);
            };
            let current = self.repos.resolve_repo(command.repo.clone())?;
            if RepoSnapshotToken::from_entry(&current.registry_entry) == token {
                return Ok(SymbolQueryOutput::Resolved(pack));
            }
            if last_attempt {
                return Err(AppError::Unavailable {
                    dependency: "repository registry",
                    message: "repository publication changed twice during the doc pack \
                              build; retry once indexing settles"
                        .into(),
                    retryable: true,
                });
            }
        }
        unreachable!("the retry loop always returns")
    }

    async fn build_pack(
        &self,
        context: &Arc<RepoContext>,
        command: &DocPackCommand,
    ) -> Result<SymbolQueryOutput<DocPackOutput>, AppError> {
        let node = match resolve_symbol(&context.store, &command.name).await? {
            SymbolResolution::Id(id) => context
                .store
                .get_node(&id)
                .await
                .map_err(|e| AppError::from_graph_store(e, "node"))?
                .ok_or_else(|| AppError::NotFound {
                    entity: "symbol",
                    key: command.name.clone(),
                })?,
            SymbolResolution::Ambiguous(nodes) => {
                return Ok(SymbolQueryOutput::Ambiguous(AmbiguousResult::from_nodes(
                    nodes,
                )));
            }
            SymbolResolution::NotFound => {
                return Err(AppError::NotFound {
                    entity: "symbol",
                    key: command.name.clone(),
                });
            }
        };
        if !kind_supported(node.kind) {
            return Err(AppError::InvalidInput {
                field: "name",
                message: format!(
                    "'{}' is a {} node; doc_pack supports Route, Method, Function, \
                     Constructor, Class, and Interface",
                    node.id.as_str(),
                    node.kind.label()
                ),
            });
        }

        let requested_profile = command.profile();
        let mut profile = requested_profile.clone();
        let mut bundle = self.build_evidence(context, &node, &profile).await?;
        let repo_name = context.repo.registry_entry.name.clone();
        let graph_version = context
            .repo
            .registry_entry
            .published_graph_content_version
            .clone();
        let node_id = node.id.as_str().to_string();

        // Byte backstop: drop whole sections in DROP_ORDER, then re-derive
        // profile → fingerprint → hash → markdown so the page describes
        // exactly what was delivered.
        loop {
            let hash = evidence_hash(&build_fingerprint(&node_id, &profile, &bundle))?;
            let markdown = render_doc_page(&render::RenderInput {
                node_id: &node_id,
                evidence_hash: &hash,
                graph_version: graph_version.as_deref(),
                profile: &profile,
                requested_profile: &requested_profile,
                identity: &bundle.identity,
                flow: &bundle.flow,
                upstream: &bundle.upstream,
                tests: &bundle.tests,
                source: &bundle.source,
                contracts: &bundle.contracts,
            });
            let output = DocPackOutput {
                repo: repo_name.clone(),
                node_id: node_id.clone(),
                requested_profile: requested_profile.clone(),
                profile: profile.clone(),
                evidence_hash: hash,
                graph_version: graph_version.clone(),
                identity: bundle.identity.clone(),
                flow: bundle.flow.to_section("graph"),
                upstream: bundle.upstream.to_section("graph"),
                tests: bundle.tests.to_section("graph"),
                source: bundle.source.to_section("file"),
                contracts: bundle.contracts.to_section("artifact"),
                markdown,
                warnings: bundle.warnings.clone(),
            };
            let size = serde_json::to_vec(&output)
                .map_err(|error| AppError::Unavailable {
                    dependency: "response serialization",
                    message: error.to_string(),
                    retryable: false,
                })?
                .len();
            if size + BACKSTOP_MARGIN_BYTES <= DOC_PACK_MAX_RESPONSE_BYTES {
                return Ok(SymbolQueryOutput::Resolved(output));
            }
            let Some(victim) = DROP_ORDER
                .into_iter()
                .find(|section| profile.sections.contains(section))
            else {
                return Err(AppError::Unavailable {
                    dependency: "doc_pack response",
                    message: format!(
                        "identity/metadata alone exceed the {DOC_PACK_MAX_RESPONSE_BYTES}-byte \
                         response cap"
                    ),
                    retryable: false,
                });
            };
            profile.sections.retain(|section| *section != victim);
            bundle.drop_section(victim);
            bundle.warnings.push(format!(
                "response byte cap (~64KB) reached — dropped section '{}'; re-fetch it with \
                 the {} tool (requested_profile preserves the original selection)",
                victim.name(),
                victim.refetch_tool()
            ));
        }
    }

    /// Build the profile-selected evidence for one node against one resolved
    /// context. Shared verbatim by `doc_pack` (with rendering) and
    /// `doc_status` (hash-only rebuilds).
    async fn build_evidence(
        &self,
        context: &Arc<RepoContext>,
        node: &Node,
        profile: &EvidenceProfileV1,
    ) -> Result<EvidenceBundle, AppError> {
        let mut warnings = Vec::new();
        let identity = identity_from_node(node, &mut warnings);
        let mut bundle = EvidenceBundle {
            identity,
            flow: SectionState::Off,
            upstream: SectionState::Off,
            tests: SectionState::Off,
            source: SectionState::Off,
            contracts: SectionState::Off,
            warnings,
            had_runtime_error: false,
        };
        for section in &profile.sections {
            match section {
                DocSection::Flow => bundle.flow = self.build_flow(context, node).await,
                DocSection::Upstream => bundle.upstream = self.build_upstream(context, node).await,
                DocSection::Tests => bundle.tests = self.build_tests(context, node).await,
                DocSection::Source => bundle.source = self.build_source(context, node).await,
                DocSection::Contracts => {
                    bundle.contracts = self
                        .build_contracts(context, node, profile.group.as_deref(), &bundle.identity)
                        .await
                }
            }
        }
        bundle.had_runtime_error = bundle.flow.runtime_error_reason().is_some()
            || bundle.upstream.runtime_error_reason().is_some()
            || bundle.tests.runtime_error_reason().is_some()
            || bundle.source.runtime_error_reason().is_some()
            || bundle.contracts.runtime_error_reason().is_some();
        Ok(bundle)
    }

    async fn build_flow(&self, context: &Arc<RepoContext>, node: &Node) -> SectionState<FlowBody> {
        if matches!(node.kind, NodeKind::Class | NodeKind::Interface) {
            return SectionState::Unavailable {
                code: UnavailableCode::MemberRequired,
                reason: "execution flow starts at a callable — this node is a type".into(),
                remedy: Some(format!(
                    "run trace_flow(entry_point=...) on one of {}'s member methods",
                    node.name
                )),
            };
        }
        let command = TraceFlowCommand {
            repo: String::new(),
            entry_point: node.id.as_str().to_string(),
            max_depth: FLOW_MAX_DEPTH,
            exclude_kinds: Vec::new(),
            business_only: true,
            max_nodes: FLOW_MAX_NODES,
            offset: 0,
        };
        match self.graph.trace_flow_in_context(context, command).await {
            Ok(SymbolQueryOutput::Resolved(trace)) => {
                let mut steps: Vec<FlowHop> = trace
                    .steps
                    .into_iter()
                    .map(|mut hop| {
                        // Keep the hop's node and edge kind; clear call-site
                        // argument text (size control — the skeleton renders
                        // structure, not argument evidence).
                        if let Some(via) = hop.via.as_mut() {
                            via.call_sites = Vec::new();
                        }
                        hop
                    })
                    .collect();
                steps.sort_by(|a, b| {
                    (a.node.depth, a.node.id.as_str()).cmp(&(b.node.depth, b.node.id.as_str()))
                });
                let mut db_effects = trace.db_effects;
                db_effects.sort_by(|a, b| {
                    (
                        a.method.as_str(),
                        a.query.as_str(),
                        a.table.as_str(),
                        a.access.as_str(),
                    )
                        .cmp(&(
                            b.method.as_str(),
                            b.query.as_str(),
                            b.table.as_str(),
                            b.access.as_str(),
                        ))
                });
                SectionState::Ok {
                    body: FlowBody {
                        steps,
                        db_effects,
                        completeness: trace.completeness,
                    },
                }
            }
            // A full NodeId never resolves ambiguously; treat it as a serving
            // fault if it somehow does.
            Ok(SymbolQueryOutput::Ambiguous(_)) => SectionState::Unavailable {
                code: UnavailableCode::RuntimeError,
                reason: "flow entry resolution returned ambiguous candidates for a full NodeId"
                    .into(),
                remedy: None,
            },
            Err(error) => runtime_error_section("flow query", &error),
        }
    }

    async fn build_upstream(
        &self,
        context: &Arc<RepoContext>,
        node: &Node,
    ) -> SectionState<UpstreamBody> {
        let filter = ContextFilter {
            caller_limit: UPSTREAM_MAX_CALLERS,
            callee_limit: 1,
            process_limit: UPSTREAM_MAX_PROCESSES,
            ..ContextFilter::default()
        };
        match context.store.context_page(&node.id, &filter).await {
            Ok(page) => {
                let mut callers: Vec<SymbolRef> = page
                    .callers
                    .items
                    .iter()
                    .map(SymbolRef::from_node)
                    .collect();
                callers.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
                let mut processes = page.processes.items;
                processes.sort();
                let mut reasons: Vec<&'static str> = Vec::new();
                if page.callers.has_more {
                    reasons.push("caller_limit");
                }
                if page.processes.has_more {
                    reasons.push("process_limit");
                }
                let completeness = ResultBounds {
                    complete: reasons.is_empty(),
                    total_known: None,
                    returned: callers.len() + processes.len(),
                    omitted: None,
                    failed: 0,
                    limit: Some(UPSTREAM_MAX_CALLERS),
                    reasons,
                };
                SectionState::Ok {
                    body: UpstreamBody {
                        callers,
                        processes,
                        completeness,
                    },
                }
            }
            Err(error) => {
                let error = AppError::from_graph_store(error, "node");
                runtime_error_section("upstream query", &error)
            }
        }
    }

    async fn build_tests(
        &self,
        context: &Arc<RepoContext>,
        node: &Node,
    ) -> SectionState<TestsBody> {
        let scope = match node.kind {
            NodeKind::Class | NodeKind::Interface => TestScope::DirectAndMembers,
            NodeKind::Method | NodeKind::Constructor => TestScope::DirectAndOwner,
            _ => TestScope::Direct,
        };
        match self
            .testing
            .test_coverage_page_in_context(context, &node.id, TESTS_MAX)
            .await
        {
            Ok(page) => {
                let mut tests: Vec<SymbolRef> =
                    page.tests.iter().map(SymbolRef::from_node).collect();
                tests.sort_by(|a, b| (&a.file, &a.name, &a.id).cmp(&(&b.file, &b.name, &b.id)));
                let completeness = if page.has_more {
                    ResultBounds::backend_limited(tests.len(), TESTS_MAX)
                } else {
                    ResultBounds::exact_limit(tests.len(), tests.len(), Some(TESTS_MAX))
                };
                SectionState::Ok {
                    body: TestsBody {
                        scope,
                        test_count: tests.len(),
                        tests,
                        completeness,
                    },
                }
            }
            Err(error) => runtime_error_section("test coverage query", &error),
        }
    }

    async fn build_source(
        &self,
        context: &Arc<RepoContext>,
        node: &Node,
    ) -> SectionState<SourceSpan> {
        if node.file.trim().is_empty() {
            return SectionState::Unavailable {
                code: UnavailableCode::RuntimeError,
                reason: "node records no source file".into(),
                remedy: Some("re-run `cih-engine analyze` if this symbol should have one".into()),
            };
        }
        let start_line = node.range.start_line.max(1);
        let end_line = if node.range.end_line == 0 {
            0
        } else {
            node.range.end_line.max(start_line)
        };
        let command = SourceSpanCommand {
            path: node.file.clone(),
            start_line,
            end_line,
            max_lines: SOURCE_MAX_LINES,
            max_bytes: SOURCE_MAX_BYTES,
        };
        match self.files.read_span_in_context(context, command).await {
            Ok(span) => SectionState::Ok { body: span },
            Err(error) => runtime_error_section("source read", &error),
        }
    }

    async fn build_contracts(
        &self,
        context: &Arc<RepoContext>,
        node: &Node,
        group: Option<&str>,
        identity: &IdentityBody,
    ) -> SectionState<ContractsBody> {
        if node.kind != NodeKind::Route {
            return SectionState::Unavailable {
                code: UnavailableCode::RoutesOnly,
                reason: "cross-repo consumers apply only to Route nodes".into(),
                remedy: Some("call doc_pack on a Route (see route_map for the list)".into()),
            };
        }
        let Some(group) = group else {
            return SectionState::Unavailable {
                code: UnavailableCode::GroupRequired,
                reason: "no `group` was supplied, so cross-repo contract consumers cannot be \
                         scoped"
                    .into(),
                remedy: Some(
                    "pass group=\"<group-name>\" (see list_repos / group registry)".into(),
                ),
            };
        };
        let (Some(method), Some(path)) = (identity.http_method.clone(), identity.path.clone())
        else {
            return SectionState::Unavailable {
                code: UnavailableCode::MissingRouteProps,
                reason: "this Route node lacks httpMethod/path properties".into(),
                remedy: Some(format!(
                    "re-run `cih-engine analyze {}` to repopulate route metadata",
                    context.repo.registry_entry.path
                )),
            };
        };
        let query = RouteConsumersQuery {
            group: group.to_string(),
            provider_repo: context.repo.registry_entry.name.clone(),
            provider_route: node.id.as_str().to_string(),
            method,
            path,
            limit: CONTRACTS_MAX_CONSUMERS,
        };
        match self.contracts.route_consumers(query).await {
            Ok(projection) => {
                let completeness = if projection.complete {
                    ResultBounds::exact_limit(
                        projection.consumers.len(),
                        projection.consumers.len(),
                        Some(CONTRACTS_MAX_CONSUMERS),
                    )
                } else {
                    ResultBounds::backend_limited(
                        projection.consumers.len(),
                        CONTRACTS_MAX_CONSUMERS,
                    )
                };
                SectionState::Ok {
                    body: ContractsBody {
                        consumers: projection.consumers,
                        completeness,
                        contracts_synced_at: projection.contracts_synced_at,
                        contracts_stale: projection.contracts_stale,
                    },
                }
            }
            Err(error) => runtime_error_section("contract scan", &error),
        }
    }
}

// ---- doc_status -------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct DocStatusCommand {
    repo: RepoSelector,
    docs_dir: String,
    max_pages: usize,
}

impl DocStatusCommand {
    pub(crate) fn try_new(
        repo: String,
        docs_dir: String,
        max_pages: usize,
    ) -> Result<Self, AppError> {
        let docs_dir = {
            let trimmed = docs_dir.trim();
            if trimmed.is_empty() {
                "docs".to_string()
            } else {
                trimmed.to_string()
            }
        };
        let candidate = Path::new(&docs_dir);
        if candidate.is_absolute()
            || candidate
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(AppError::InvalidInput {
                field: "docs_dir",
                message: "must be a repo-relative path without '..' components".into(),
            });
        }
        let max_pages = if max_pages == 0 {
            DOC_STATUS_DEFAULT_MAX_PAGES
        } else {
            max_pages.clamp(1, DOC_STATUS_MAX_PAGES)
        };
        Ok(Self {
            repo: RepoSelector::from_wire(&repo),
            docs_dir,
            max_pages,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DocStatus {
    Fresh,
    Stale,
    MissingNode,
    Unparseable,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DocStatusPage {
    /// Repo-root-relative page path.
    pub(crate) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) node_id: Option<String>,
    pub(crate) status: DocStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stored_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) current_hash: Option<String>,
    /// True when a prior byte backstop reduced the delivered profile below the
    /// requested one — such a page can legitimately stay fresh forever under
    /// its reduced profile; regenerate from `cih_requested_profile` to retry
    /// the full pack.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) profile_reduced: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DocStatusOutput {
    pub(crate) repo: String,
    pub(crate) docs_dir: String,
    pub(crate) pages: Vec<DocStatusPage>,
    pub(crate) completeness: ResultBounds,
}

/// One parsed CIH page awaiting comparison.
#[derive(Clone, Debug)]
struct PageCandidate {
    path: String,
    node_id: String,
    stored_hash: String,
    profile: EvidenceProfileV1,
    requested_profile: EvidenceProfileV1,
}

struct DocsScan {
    candidates: Vec<PageCandidate>,
    unparseable: Vec<DocStatusPage>,
    entries_capped: bool,
    pages_capped: bool,
    docs_dir_missing: bool,
}

enum RebuildOutcome {
    Hash(String),
    MissingNode,
    Error(String),
}

impl DocPackService {
    pub(crate) async fn status(
        &self,
        command: DocStatusCommand,
    ) -> Result<DocStatusOutput, AppError> {
        let context = self.repos.resolve(command.repo.clone()).await?;
        let token = RepoSnapshotToken::from_entry(&context.repo.registry_entry);
        let repo_name = context.repo.registry_entry.name.clone();
        let root = context.repo.canonical_path.clone();
        let docs_dir = command.docs_dir.clone();
        let max_pages = command.max_pages;
        let scan = run_blocking(blocking_timeout(), "doc_status scan", move || {
            scan_docs(&root, &docs_dir, max_pages)
        })
        .await
        .map_err(|error| AppError::Unavailable {
            dependency: "doc_status scan",
            message: error.to_string(),
            retryable: true,
        })??;

        // One evidence rebuild per unique (node, profile) pair, with small
        // bounded concurrency — never one task per page.
        let mut unique: BTreeMap<(String, String), (String, EvidenceProfileV1)> = BTreeMap::new();
        for candidate in &scan.candidates {
            let profile_key = serde_json::to_string(&candidate.profile).map_err(|error| {
                AppError::Unavailable {
                    dependency: "profile serialization",
                    message: error.to_string(),
                    retryable: false,
                }
            })?;
            unique
                .entry((candidate.node_id.clone(), profile_key))
                .or_insert_with(|| (candidate.node_id.clone(), candidate.profile.clone()));
        }
        let semaphore = Arc::new(tokio::sync::Semaphore::new(DOC_STATUS_REBUILD_CONCURRENCY));
        let mut tasks = tokio::task::JoinSet::new();
        for (key, (node_id, profile)) in unique {
            let service = self.clone();
            let context = context.clone();
            let semaphore = semaphore.clone();
            tasks.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("doc_status semaphore never closes");
                let outcome = service.rebuild_hash(&context, &node_id, &profile).await;
                (key, outcome)
            });
        }
        let mut outcomes: BTreeMap<(String, String), RebuildOutcome> = BTreeMap::new();
        while let Some(joined) = tasks.join_next().await {
            let (key, outcome) = joined.map_err(|error| AppError::Unavailable {
                dependency: "doc_status rebuild",
                message: error.to_string(),
                retryable: true,
            })?;
            outcomes.insert(key, outcome);
        }

        let mut pages = scan.unparseable;
        for candidate in scan.candidates {
            let profile_key =
                serde_json::to_string(&candidate.profile).expect("profile serialized above");
            let outcome = outcomes
                .get(&(candidate.node_id.clone(), profile_key))
                .expect("every candidate pair was rebuilt");
            let profile_reduced = (candidate.profile.sections
                != candidate.requested_profile.sections
                || candidate.profile.include_source != candidate.requested_profile.include_source
                || candidate.profile.group != candidate.requested_profile.group)
                .then_some(true);
            let page = match outcome {
                RebuildOutcome::Hash(current) => DocStatusPage {
                    path: candidate.path,
                    node_id: Some(candidate.node_id),
                    status: if *current == candidate.stored_hash {
                        DocStatus::Fresh
                    } else {
                        DocStatus::Stale
                    },
                    stored_hash: Some(candidate.stored_hash),
                    current_hash: Some(current.clone()),
                    profile_reduced,
                    reason: None,
                },
                RebuildOutcome::MissingNode => DocStatusPage {
                    path: candidate.path,
                    node_id: Some(candidate.node_id.clone()),
                    status: DocStatus::MissingNode,
                    stored_hash: Some(candidate.stored_hash),
                    current_hash: None,
                    profile_reduced,
                    reason: Some(format!(
                        "node '{}' no longer exists in the graph",
                        candidate.node_id
                    )),
                },
                RebuildOutcome::Error(reason) => DocStatusPage {
                    path: candidate.path,
                    node_id: Some(candidate.node_id),
                    status: DocStatus::Error,
                    stored_hash: Some(candidate.stored_hash),
                    current_hash: None,
                    profile_reduced,
                    reason: Some(reason.clone()),
                },
            };
            pages.push(page);
        }
        pages.sort_by(|a, b| a.path.cmp(&b.path));

        // A publication change during the batch would mix hash generations;
        // abort retryably instead of emitting a mixed report.
        let current = self.repos.resolve_repo(command.repo.clone())?;
        if RepoSnapshotToken::from_entry(&current.registry_entry) != token {
            return Err(AppError::Unavailable {
                dependency: "repository registry",
                message: "repository publication changed during the status scan; retry".into(),
                retryable: true,
            });
        }

        let mut reasons: Vec<&'static str> = Vec::new();
        if scan.pages_capped {
            reasons.push("page_limit");
        }
        if scan.entries_capped {
            reasons.push("walk_entry_limit");
        }
        if scan.docs_dir_missing {
            reasons.push("docs_dir_missing");
        }
        let returned = pages.len();
        Ok(DocStatusOutput {
            repo: repo_name,
            docs_dir: command.docs_dir,
            pages,
            completeness: ResultBounds {
                complete: !scan.pages_capped && !scan.entries_capped,
                total_known: None,
                returned,
                omitted: None,
                failed: 0,
                limit: Some(command.max_pages),
                reasons,
            },
        })
    }

    /// Hash-only rebuild for one `(node, profile)` pair. This is the internal
    /// entry point that accepts an already-normalized profile — including a
    /// backstop-produced empty effective section list, which
    /// `DocPackCommand::try_new`'s caller-input contract correctly rejects.
    async fn rebuild_hash(
        &self,
        context: &Arc<RepoContext>,
        node_id: &str,
        profile: &EvidenceProfileV1,
    ) -> RebuildOutcome {
        let id = NodeId::new(node_id.to_string());
        let node = match context.store.get_node(&id).await {
            Ok(Some(node)) => node,
            Ok(None) => return RebuildOutcome::MissingNode,
            Err(error) => {
                let error = AppError::from_graph_store(error, "node");
                return RebuildOutcome::Error(format!("node lookup failed: {error}"));
            }
        };
        if !kind_supported(node.kind) {
            return RebuildOutcome::Error(format!(
                "node kind {} is not supported by doc_pack",
                node.kind.label()
            ));
        }
        let bundle = match self.build_evidence(context, &node, profile).await {
            Ok(bundle) => bundle,
            Err(error) => return RebuildOutcome::Error(error.to_string()),
        };
        if bundle.had_runtime_error {
            // Never claim freshness from a partial rebuild.
            let detail = [
                ("flow", &bundle.flow as &dyn SectionErrorText),
                ("upstream", &bundle.upstream),
                ("tests", &bundle.tests),
                ("source", &bundle.source),
                ("contracts", &bundle.contracts),
            ]
            .iter()
            .find_map(|(name, state)| state.runtime_error_reason().map(|r| format!("{name}: {r}")))
            .unwrap_or_else(|| "a requested section failed".to_string());
            return RebuildOutcome::Error(detail);
        }
        match evidence_hash(&build_fingerprint(node_id, profile, &bundle)) {
            Ok(hash) => RebuildOutcome::Hash(hash),
            Err(error) => RebuildOutcome::Error(error.to_string()),
        }
    }
}

/// Erased view over `SectionState<T>` so `rebuild_hash` can report which
/// section failed without a generic helper per body type.
trait SectionErrorText {
    fn runtime_error_reason(&self) -> Option<&str>;
}

impl<T> SectionErrorText for SectionState<T> {
    fn runtime_error_reason(&self) -> Option<&str> {
        match self {
            SectionState::Unavailable {
                code: UnavailableCode::RuntimeError,
                reason,
                ..
            } => Some(reason),
            _ => None,
        }
    }
}

// ---- docs directory scan ----------------------------------------------------

/// Deterministic-walk bounds, injectable so tests exercise the caps without
/// creating ten thousand fixture files.
#[derive(Clone, Copy)]
struct WalkCaps {
    max_depth: usize,
    max_entries: usize,
}

impl Default for WalkCaps {
    fn default() -> Self {
        Self {
            max_depth: DOC_STATUS_WALK_MAX_DEPTH,
            max_entries: DOC_STATUS_WALK_MAX_ENTRIES,
        }
    }
}

fn scan_docs(repo_root: &Path, docs_dir: &str, max_pages: usize) -> Result<DocsScan, AppError> {
    scan_docs_with_caps(repo_root, docs_dir, max_pages, WalkCaps::default())
}

fn scan_docs_with_caps(
    repo_root: &Path,
    docs_dir: &str,
    max_pages: usize,
    caps: WalkCaps,
) -> Result<DocsScan, AppError> {
    use crate::application::files::{canonical_contained_target, ContainmentError};

    let docs_root = match canonical_contained_target(repo_root, &repo_root.join(docs_dir)) {
        Ok(root) => root,
        Err(ContainmentError::Target(_)) => {
            // No docs directory yet is an ordinary answer, not an error.
            return Ok(DocsScan {
                candidates: Vec::new(),
                unparseable: Vec::new(),
                entries_capped: false,
                pages_capped: false,
                docs_dir_missing: true,
            });
        }
        Err(ContainmentError::Root(error)) => {
            return Err(AppError::InvalidInput {
                field: "docs_dir",
                message: format!("cannot resolve repo root: {error}"),
            });
        }
        Err(ContainmentError::Outside) => {
            return Err(AppError::InvalidInput {
                field: "docs_dir",
                message: format!("'{docs_dir}' resolves outside the repository root"),
            });
        }
    };
    if !docs_root.is_dir() {
        return Err(AppError::InvalidInput {
            field: "docs_dir",
            message: format!("'{docs_dir}' is not a directory"),
        });
    }

    let mut scan = DocsScan {
        candidates: Vec::new(),
        unparseable: Vec::new(),
        entries_capped: false,
        pages_capped: false,
        docs_dir_missing: false,
    };
    let mut visited_entries = 0usize;
    let mut candidate_count = 0usize;
    // Deterministic, symlink-free, depth-limited walk. Directories queue in
    // sorted order so the page cap always cuts at the same place.
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> =
        std::collections::VecDeque::from([(docs_root.clone(), 0)]);
    'walk: while let Some((dir, depth)) = queue.pop_front() {
        let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries.filter_map(|entry| entry.ok()).collect(),
            Err(_) => continue,
        };
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            visited_entries += 1;
            if visited_entries > caps.max_entries {
                scan.entries_capped = true;
                break 'walk;
            }
            let entry_path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&entry_path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                if depth + 1 < caps.max_depth {
                    queue.push_back((entry_path, depth + 1));
                }
                continue;
            }
            if entry_path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let relative = entry_path
                .strip_prefix(&docs_root)
                .map(|suffix| Path::new(docs_dir).join(suffix))
                .unwrap_or_else(|_| entry_path.clone())
                .to_string_lossy()
                .into_owned();
            match parse_page(&entry_path, &relative) {
                ParsedPage::NotCih => {}
                ParsedPage::Candidate(candidate) => {
                    candidate_count += 1;
                    if candidate_count > max_pages {
                        // The over-fetched candidate only proves incompleteness.
                        scan.pages_capped = true;
                        break 'walk;
                    }
                    scan.candidates.push(candidate);
                }
                ParsedPage::Unparseable(page) => {
                    candidate_count += 1;
                    if candidate_count > max_pages {
                        scan.pages_capped = true;
                        break 'walk;
                    }
                    scan.unparseable.push(page);
                }
            }
        }
    }
    Ok(scan)
}

enum ParsedPage {
    NotCih,
    Candidate(PageCandidate),
    Unparseable(DocStatusPage),
}

fn unparseable(path: &str, node_id: Option<String>, reason: impl Into<String>) -> ParsedPage {
    ParsedPage::Unparseable(DocStatusPage {
        path: path.to_string(),
        node_id,
        status: DocStatus::Unparseable,
        stored_hash: None,
        current_hash: None,
        profile_reduced: None,
        reason: Some(reason.into()),
    })
}

/// Read one page's frontmatter through a byte-limited reader and classify it.
/// Ordinary Markdown (no frontmatter, or a complete block with no `cih_` key)
/// is ignored; any `cih_` key makes full CIH metadata mandatory.
fn parse_page(path: &Path, relative: &str) -> ParsedPage {
    use std::io::Read;

    let Ok(file) = std::fs::File::open(path) else {
        return unparseable(relative, None, "cannot open file");
    };
    // One extra byte distinguishes "fits exactly" from "over the cap".
    let mut header = Vec::with_capacity(8 * 1024);
    let took = std::io::BufReader::new(file)
        .take(DOC_STATUS_FRONTMATTER_MAX_BYTES as u64 + 1)
        .read_to_end(&mut header);
    if took.is_err() {
        return unparseable(relative, None, "cannot read file header");
    }
    let capped = header.len() > DOC_STATUS_FRONTMATTER_MAX_BYTES;
    let text = String::from_utf8_lossy(&header);
    let mut lines = text.lines();
    if lines.next().map(|line| line.trim_end()) != Some("---") {
        return ParsedPage::NotCih;
    }
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    let mut any_cih_key = false;
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.starts_with("cih_") {
            any_cih_key = true;
            fields.insert(key, value.trim());
        }
    }
    if !closed {
        if capped {
            // A possible CIH page must not be silently ignored just because
            // its frontmatter is enormous.
            return unparseable(relative, None, "frontmatter_too_large");
        }
        return if any_cih_key {
            unparseable(relative, None, "unterminated frontmatter")
        } else {
            ParsedPage::NotCih
        };
    }
    if !any_cih_key {
        return ParsedPage::NotCih;
    }

    let node_id = match fields
        .get("cih_node")
        .map(|raw| serde_json::from_str::<String>(raw))
    {
        Some(Ok(node_id)) if !node_id.trim().is_empty() => node_id,
        Some(_) => return unparseable(relative, None, "cih_node is not a JSON string"),
        None => return unparseable(relative, None, "missing cih_node"),
    };
    let Some(stored_hash) = fields.get("cih_evidence_hash").map(|raw| raw.to_string()) else {
        return unparseable(relative, Some(node_id), "missing cih_evidence_hash");
    };
    if stored_hash.len() != 32
        || !stored_hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    {
        return unparseable(
            relative,
            Some(node_id),
            "cih_evidence_hash is not 32 lowercase hex characters",
        );
    }
    match fields.get("cih_generator") {
        Some(&DOC_GENERATOR) => {}
        Some(other) => {
            return unparseable(
                relative,
                Some(node_id),
                format!("unsupported cih_generator '{other}'"),
            );
        }
        None => return unparseable(relative, Some(node_id), "missing cih_generator"),
    }
    let profile = match fields
        .get("cih_profile")
        .map(|raw| EvidenceProfileV1::parse(raw))
    {
        Some(Ok(profile)) => profile,
        Some(Err(reason)) => {
            return unparseable(relative, Some(node_id), format!("cih_profile: {reason}"));
        }
        None => return unparseable(relative, Some(node_id), "missing cih_profile"),
    };
    let requested_profile = match fields
        .get("cih_requested_profile")
        .map(|raw| EvidenceProfileV1::parse(raw))
    {
        Some(Ok(profile)) if !profile.sections.is_empty() => profile,
        Some(Ok(_)) => {
            return unparseable(
                relative,
                Some(node_id),
                "cih_requested_profile has no sections",
            );
        }
        Some(Err(reason)) => {
            return unparseable(
                relative,
                Some(node_id),
                format!("cih_requested_profile: {reason}"),
            );
        }
        None => return unparseable(relative, Some(node_id), "missing cih_requested_profile"),
    };
    // `cih_graph_version` is optional diagnostics-only provenance.
    ParsedPage::Candidate(PageCandidate {
        path: relative.to_string(),
        node_id,
        stored_hash,
        profile,
        requested_profile,
    })
}

#[cfg(test)]
mod tests;
