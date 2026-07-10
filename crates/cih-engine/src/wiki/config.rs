use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use cih_core::{Edge, Node, RepoMap};
use cih_wiki::{ClassEnrichmentStore, WikiGraph, WikiMeta};

use crate::llm::{LlmAdapter, LlmCallConfig};

/// Wiki generation mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WikiMode {
    #[default]
    Graph,
    LlmSummary,
    LlmFull,
}

impl std::fmt::Display for WikiMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Graph => "graph",
            Self::LlmSummary => "llm-summary",
            Self::LlmFull => "llm-full",
        })
    }
}

impl std::str::FromStr for WikiMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "graph" => Ok(Self::Graph),
            "llm-summary" => Ok(Self::LlmSummary),
            "llm-full" => Ok(Self::LlmFull),
            other => anyhow::bail!(
                "unknown --wiki-mode '{}'; expected graph | llm-summary | llm-full",
                other
            ),
        }
    }
}

/// Wiki community-to-module grouping strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WikiGrouping {
    #[default]
    Package,
    Graph,
    Llm,
}

impl std::fmt::Display for WikiGrouping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Package => "package",
            Self::Graph => "graph",
            Self::Llm => "llm",
        })
    }
}

impl std::str::FromStr for WikiGrouping {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "package" => Ok(Self::Package),
            "graph" => Ok(Self::Graph),
            "llm" => Ok(Self::Llm),
            other => anyhow::bail!(
                "unknown --grouping '{}'; expected package | graph | llm",
                other
            ),
        }
    }
}

/// Increment this whenever any LLM prompt template changes so that cached outputs
/// produced with old prompts are automatically invalidated.
pub(super) const PROMPT_VERSION: u32 = 1;

pub(super) fn fnv64(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

/// Composite LLM cache key: combines evidence content with model, language, and
/// prompt version so that switching provider/model/language/prompt invalidates cache.
pub(super) fn llm_cache_key(evidence: &str, model: &str, language: &str) -> String {
    fnv64(&format!("{}\x00{}\x00{}\x00{}", evidence, model, language, PROMPT_VERSION))
}

pub(super) fn load_wiki_meta(out_dir: &Path) -> Option<WikiMeta> {
    let path = out_dir.join("wiki_meta.json");
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub(super) fn load_class_enrichment(cih_dir: &Path) -> ClassEnrichmentStore {
    let path = cih_dir.join("class-enrichment.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return ClassEnrichmentStore::default(),
    };
    serde_json::from_str(&text).unwrap_or_default()
}

pub(super) fn save_class_enrichment(cih_dir: &Path, store: &ClassEnrichmentStore) -> Result<()> {
    std::fs::create_dir_all(cih_dir)?;
    let tmp = cih_dir.join("class-enrichment.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(store)?)?;
    std::fs::rename(&tmp, cih_dir.join("class-enrichment.json"))?;
    Ok(())
}

pub struct WikiConfig {
    pub repo: PathBuf,
    pub out: Option<PathBuf>,
    pub run_llm: bool,
    pub llm: LlmCallConfig,
    pub llm_provider_config: Option<PathBuf>,
    pub evidence_paths: Vec<PathBuf>,
    pub llm_concurrency: usize,
    pub llm_debug_evidence: bool,
    pub llm_dry_run: bool,
    pub wiki_language: String,
    pub wiki_mode: WikiMode,
    pub grouping: WikiGrouping,
    pub html: bool,
    pub incremental: bool,
    pub save_evidence: bool,
    pub filter_community: Vec<String>,
    pub max_communities: Option<usize>,
    pub filter_feature: Vec<String>,
    pub filter_route: Vec<String>,
    pub json: bool,
    /// Only check whether the wiki is up to date; do not regenerate.
    /// Exits 0 if up to date, exits 2 if stale.
    pub check_only: bool,
    /// Re-render only features affected by files changed since this git ref.
    /// Requires a previous full wiki run (manifest.json) to merge unchanged feature pages.
    pub since_ref: Option<String>,
}

impl Default for WikiConfig {
    fn default() -> Self {
        Self {
            repo: PathBuf::new(),
            out: None,
            run_llm: false,
            llm: LlmCallConfig::default(),
            llm_provider_config: None,
            evidence_paths: vec![],
            llm_concurrency: 4,
            llm_debug_evidence: false,
            llm_dry_run: false,
            wiki_language: "en".into(),
            wiki_mode: WikiMode::Graph,
            grouping: WikiGrouping::Package,
            html: false,
            incremental: false,
            save_evidence: false,
            filter_community: vec![],
            max_communities: None,
            filter_feature: vec![],
            filter_route: vec![],
            json: false,
            check_only: false,
            since_ref: None,
        }
    }
}

pub(super) struct LlmRunParams<'a> {
    pub adapter: &'a dyn LlmAdapter,
    pub api_key: Option<&'a str>,
    pub model: &'a str,
    pub max_tokens: u32,
    pub timeout_secs: u64,
    pub retries: u32,
    pub dry_run: bool,
    pub language: &'a str,
    pub debug_evidence: bool,
}

pub(super) struct WikiArtifacts {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub wiki_graph: WikiGraph,
    pub community_nodes: Vec<Node>,
    pub community_edges: Vec<Edge>,
    pub community_version: String,
    pub graph_version: String,
    pub repo_map: Option<RepoMap>,
    pub unresolved_report: Option<String>,
    pub out_dir: PathBuf,
    pub repo_name: String,
    pub bodies: HashMap<String, cih_wiki::BodyEntry>,
    pub file_dev_map: HashMap<String, String>,
    #[allow(clippy::type_complexity)] // LLM plumbing signature; alias with wiki rework
    pub feature_of: Box<dyn Fn(&str, &str) -> String + Send>,
}
