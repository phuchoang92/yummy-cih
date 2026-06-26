use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cih_core::{Edge, Node, RepoMap};
use cih_wiki::{ClassEnrichmentStore, WikiGraph, WikiMeta};

use crate::llm::LlmAdapter;

pub(super) fn fnv64(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
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
    pub llm_provider: String,
    pub llm_provider_config: Option<PathBuf>,
    pub llm_api_key_env: Option<String>,
    pub evidence_paths: Vec<PathBuf>,
    pub llm_base_url: String,
    pub llm_model: String,
    pub llm_max_tokens: u32,
    pub llm_timeout_secs: u64,
    pub llm_retries: u32,
    pub llm_concurrency: usize,
    pub llm_debug_evidence: bool,
    pub llm_dry_run: bool,
    pub wiki_language: String,
    pub wiki_mode: String,
    pub grouping: String,
    pub html: bool,
    pub incremental: bool,
    pub save_evidence: bool,
    pub filter_community: Vec<String>,
    pub max_communities: Option<usize>,
    pub filter_feature: Vec<String>,
    pub filter_route: Vec<String>,
    pub json: bool,
}

impl Default for WikiConfig {
    fn default() -> Self {
        Self {
            repo: PathBuf::new(),
            out: None,
            run_llm: false,
            llm_provider: "openai-compatible".into(),
            llm_provider_config: None,
            llm_api_key_env: None,
            evidence_paths: vec![],
            llm_base_url: "https://api.openai.com/v1".into(),
            llm_model: String::new(),
            llm_max_tokens: 1024,
            llm_timeout_secs: 30,
            llm_retries: 2,
            llm_concurrency: 4,
            llm_debug_evidence: false,
            llm_dry_run: false,
            wiki_language: "en".into(),
            wiki_mode: "graph".into(),
            grouping: "package".into(),
            html: false,
            incremental: false,
            save_evidence: false,
            filter_community: vec![],
            max_communities: None,
            filter_feature: vec![],
            filter_route: vec![],
            json: false,
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
    pub feature_of: Box<dyn Fn(&str, &str) -> String + Send>,
}
