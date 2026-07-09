use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path};

use anyhow::{bail, Result};
use cih_core::RepoMap;
use serde::{Deserialize, Serialize};

use crate::features::{build_dev_page_paths, group_communities_by_feature};
use crate::graph::WikiGraph;
use crate::slugify::slugify;
use crate::FlowLlmSummary;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WikiModuleTree {
    pub schema_version: u32,
    pub generated_at: String,
    pub source: ModuleTreeSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_commit: Option<String>,
    pub graph_version: String,
    pub community_version: String,
    pub modules: Vec<WikiModuleNode>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModuleTreeSource {
    Graph,
    Llm,
    UserEdited,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WikiModuleNode {
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub community_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<WikiModuleNode>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WikiMeta {
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_commit: Option<String>,
    pub graph_version: String,
    pub community_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub prompt_version: String,
    #[serde(default)]
    pub module_cache: BTreeMap<String, WikiModuleCacheEntry>,
    /// Cached feature-level LLM summaries (keyed by feature name). Added in schema v2;
    /// `#[serde(default)]` makes existing wiki_meta.json files (without this field) still load.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub feature_cache: BTreeMap<String, FeatureMetaEntry>,
    /// Cached route-flow and process-flow LLM summaries (keyed by handler/process id).
    /// Added in schema v3; `#[serde(default)]` keeps existing files loadable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub flow_cache: BTreeMap<String, FlowCacheEntry>,
}

/// Cached LLM enrichment for one class (keyed by FQCN in `ClassEnrichmentStore`).
/// Stored at `.cih/class-enrichment.json`, not inside the wiki `--out` dir,
/// so the cache survives wiki re-runs and `--out` relocations.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ClassCacheEntry {
    /// FNV-1a 64-bit hash of sorted method source bodies; used for invalidation.
    pub content_hash: String,
    /// Simple method name → one-sentence business description.
    #[serde(default)]
    pub method_descriptions: HashMap<String, String>,
    /// One-paragraph plain-text summary of what this class does.
    #[serde(default)]
    pub class_summary: String,
}

/// Top-level container serialized to `.cih/class-enrichment.json`.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ClassEnrichmentStore {
    pub schema_version: u32,
    /// FQCN → enrichment entry.
    #[serde(default)]
    pub entries: BTreeMap<String, ClassCacheEntry>,
}

/// Cached route-flow enrichment for one HTTP handler (keyed by handler_id in `WikiMeta.flow_cache`).
/// Invalidated when the call-chain text changes (FNV-1a hash of `chain_steps_text` output).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FlowCacheEntry {
    /// FNV-1a 64-bit hash of the chain_steps_text used to build the LLM prompt.
    pub evidence_hash: String,
    pub summary: FlowLlmSummary,
}

/// Cached feature-level LLM summary for one wiki feature.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FeatureMetaEntry {
    pub ev_hash: String,
    pub po_overview: String,
    pub po_capabilities: String,
    pub ba_process_overview: String,
    pub ba_business_rules: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WikiModuleCacheEntry {
    pub content_hash: String,
    pub evidence_hash: String,
    pub page_paths: Vec<String>,
    /// Cached LLM summary for incremental mode (may be absent for graph-only runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_po: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_ba: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_dev: Option<String>,
}

pub fn build_graph_module_tree(
    graph: &WikiGraph,
    repo_map: Option<&RepoMap>,
    graph_version: &str,
    community_version: &str,
    repo_commit: Option<String>,
) -> WikiModuleTree {
    let groups = group_communities_by_feature(graph);
    let dev_paths = build_dev_page_paths(&groups, graph);
    let mut modules = Vec::new();

    for group in groups {
        let mut file_paths = BTreeSet::new();
        let mut module_names = BTreeSet::new();
        for comm_id in &group.community_ids {
            if let Some(members) = graph.members_by_community.get(comm_id) {
                for member in members {
                    if !member.file.is_empty() && is_repo_relative(&member.file) {
                        file_paths.insert(member.file.clone());
                        if let Some(name) = repo_module_for_file(repo_map, &member.file) {
                            module_names.insert(name);
                        }
                    }
                }
            }
        }

        let description = if module_names.is_empty() {
            Some(format!(
                "Graph-derived module from {} communities and {} files.",
                group.community_ids.len(),
                file_paths.len()
            ))
        } else {
            Some(format!(
                "Graph-derived module from {} communities, {} files, and build modules: {}.",
                group.community_ids.len(),
                file_paths.len(),
                module_names.into_iter().collect::<Vec<_>>().join(", ")
            ))
        };

        let children = group
            .community_ids
            .iter()
            .map(|comm_id| {
                let comm_name = graph.community_name(comm_id).to_string();
                let page_slug = dev_paths
                    .get(comm_id)
                    .and_then(|p| p.rsplit('/').next())
                    .map(str::to_string)
                    .unwrap_or_else(|| slugify(&comm_name));
                let display_title = title_from_slug(&page_slug);
                WikiModuleNode {
                    id: format!("module:{}:{}", group.feature, slugify(comm_id)),
                    slug: page_slug,
                    title: display_title,
                    description: Some(format!("Community {}", comm_id)),
                    community_ids: vec![comm_id.to_string()],
                    file_paths: sorted_member_files(graph, comm_id),
                    children: Vec::new(),
                }
            })
            .collect();

        modules.push(WikiModuleNode {
            id: format!("feature:{}", group.feature),
            slug: group.feature.clone(),
            title: title_from_slug(&group.feature),
            description,
            community_ids: group.community_ids,
            file_paths: file_paths.into_iter().collect(),
            children,
        });
    }

    WikiModuleTree {
        schema_version: 1,
        generated_at: cih_core::now_rfc3339(),
        source: ModuleTreeSource::Graph,
        repo_commit,
        graph_version: graph_version.to_string(),
        community_version: community_version.to_string(),
        modules,
    }
}

pub fn read_module_tree(path: &Path) -> Result<WikiModuleTree> {
    let raw = std::fs::read_to_string(path)?;
    let mut tree: WikiModuleTree = serde_json::from_str(&raw)?;
    tree.source = ModuleTreeSource::UserEdited;
    Ok(tree)
}

pub fn validate_module_tree(tree: &WikiModuleTree, graph: &WikiGraph) -> Result<()> {
    if tree.schema_version != 1 {
        bail!(
            "unsupported module_tree schema_version {}; expected 1",
            tree.schema_version
        );
    }
    let valid_communities: BTreeSet<String> = graph
        .community_nodes
        .iter()
        .map(|n| n.id.as_str().to_string())
        .collect();
    let mut seen_ids = BTreeSet::new();
    // Top-level slugs must be globally unique (they map to URL prefixes).
    let mut top_level_slugs = BTreeSet::new();
    for module in &tree.modules {
        validate_module_node(
            module,
            &valid_communities,
            &mut seen_ids,
            &mut top_level_slugs,
            true,
        )?;
    }
    Ok(())
}

fn validate_module_node(
    node: &WikiModuleNode,
    valid_communities: &BTreeSet<String>,
    seen_ids: &mut BTreeSet<String>,
    seen_slugs: &mut BTreeSet<String>,
    check_slug: bool,
) -> Result<()> {
    if node.id.trim().is_empty() {
        bail!("module tree contains an empty module id");
    }
    if node.slug.trim().is_empty() {
        bail!("module '{}' contains an empty slug", node.id);
    }
    if !seen_ids.insert(node.id.clone()) {
        bail!("duplicate module id '{}'", node.id);
    }
    // Child slugs only need to be unique within their parent directory, not globally.
    if check_slug && !seen_slugs.insert(node.slug.clone()) {
        bail!("duplicate module slug '{}'", node.slug);
    }
    for community_id in &node.community_ids {
        if !valid_communities.contains(community_id) {
            bail!(
                "module '{}' references unknown community '{}'",
                node.id,
                community_id
            );
        }
    }
    let mut sibling_files = BTreeSet::new();
    for path in &node.file_paths {
        if !is_repo_relative(path) {
            bail!("module '{}' has unsafe file path '{}'", node.id, path);
        }
        if !sibling_files.insert(path) {
            bail!("module '{}' repeats file path '{}'", node.id, path);
        }
    }
    // Children: check slug uniqueness among siblings only (same parent directory).
    let mut sibling_slugs = BTreeSet::new();
    for child in &node.children {
        validate_module_node(child, valid_communities, seen_ids, &mut sibling_slugs, true)?;
    }
    Ok(())
}

pub fn build_wiki_meta(
    tree: &WikiModuleTree,
    model: Option<String>,
    language: Option<String>,
) -> WikiMeta {
    WikiMeta {
        schema_version: 1,
        repo_commit: tree.repo_commit.clone(),
        graph_version: tree.graph_version.clone(),
        community_version: tree.community_version.clone(),
        model,
        language,
        prompt_version: "phase10c-1".to_string(),
        module_cache: BTreeMap::new(),
        feature_cache: BTreeMap::new(),
        flow_cache: BTreeMap::new(),
    }
}

fn sorted_member_files(graph: &WikiGraph, community_id: &str) -> Vec<String> {
    let mut files = BTreeSet::new();
    if let Some(members) = graph.members_by_community.get(community_id) {
        for member in members {
            if !member.file.is_empty() && is_repo_relative(&member.file) {
                files.insert(member.file.clone());
            }
        }
    }
    files.into_iter().collect()
}

fn repo_module_for_file(repo_map: Option<&RepoMap>, file: &str) -> Option<String> {
    let repo_map = repo_map?;
    repo_map
        .modules
        .iter()
        .filter(|m| !m.rel_path.is_empty() && file.starts_with(&m.rel_path))
        .max_by_key(|m| m.rel_path.len())
        .map(|m| m.name.clone())
}

fn title_from_slug(slug: &str) -> String {
    slug.split('-')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            match chars.next() {
                Some(first) => {
                    let mut out = first.to_ascii_uppercase().to_string();
                    out.push_str(chars.as_str());
                    out
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_repo_relative(path: &str) -> bool {
    if path.trim().is_empty() || Path::new(path).is_absolute() {
        return false;
    }
    !Path::new(path).components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}
