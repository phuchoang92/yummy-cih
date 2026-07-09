use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WikiManifest {
    pub schema_version: u32,
    pub generated_at: String,
    pub repo_name: String,
    pub graph_version: String,
    pub community_version: String,
    pub stats: WikiStats,
    pub roles: Vec<String>,
    pub nav: BTreeMap<String, Vec<NavEntry>>,
    pub pages: Vec<PageEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm: Option<WikiLlmInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<WikiGenerationInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module_tree_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wiki_meta_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WikiLlmInfo {
    pub provider: String,
    pub model: String,
    pub language: String,
    pub evidence_file_count: usize,
    pub enriched_community_count: usize,
    pub failed_community_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_community_ids: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WikiGenerationInfo {
    pub mode: String,
    pub grouping: String,
    pub review_required: bool,
    pub html_viewer: bool,
    pub incremental: bool,
}

impl Default for WikiGenerationInfo {
    fn default() -> Self {
        Self {
            mode: "graph".to_string(),
            grouping: "graph".to_string(),
            review_required: false,
            html_viewer: false,
            incremental: false,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct WikiStats {
    pub community_count: usize,
    pub route_count: usize,
    pub process_count: usize,
    pub class_count: usize,
    pub test_class_count: usize,
    pub unresolved_ref_count: usize,
    #[serde(default)]
    pub feature_count: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NavEntry {
    pub slug: String,
    pub title: String,
    pub kind: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PageEntry {
    pub slug: String,
    pub role: String,
    pub title: String,
    pub kind: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub community_id: Option<String>,
}
