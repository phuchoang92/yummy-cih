use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

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
    pub llm_enriched: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_model: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> WikiManifest {
        WikiManifest {
            schema_version: 1,
            generated_at: "2026-06-16T10:00:00Z".to_string(),
            repo_name: "test-service".to_string(),
            graph_version: "abc123".to_string(),
            community_version: "def456".to_string(),
            stats: WikiStats {
                community_count: 2,
                route_count: 5,
                process_count: 1,
                class_count: 42,
                test_class_count: 8,
                unresolved_ref_count: 3,
                feature_count: 1,
            },
            roles: vec!["po".into(), "ba".into(), "dev".into()],
            nav: BTreeMap::new(),
            pages: vec![],
            llm_enriched: None,
            llm_model: None,
        }
    }

    #[test]
    fn manifest_round_trips_json() {
        let m = sample_manifest();
        let json = serde_json::to_string(&m).unwrap();
        let m2: WikiManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m2.schema_version, 1);
        assert_eq!(m2.repo_name, "test-service");
        assert_eq!(m2.stats.community_count, 2);
        assert_eq!(m2.roles, vec!["po", "ba", "dev"]);
    }

    #[test]
    fn manifest_llm_fields_absent_when_not_enriched() {
        let m = sample_manifest();
        let json = serde_json::to_string(&m).unwrap();
        assert!(!json.contains("llm_enriched"), "llm_enriched should be absent");
        assert!(!json.contains("llm_model"), "llm_model should be absent");
    }

    #[test]
    fn manifest_llm_fields_present_when_enriched() {
        let mut m = sample_manifest();
        m.llm_enriched = Some(true);
        m.llm_model = Some("claude-haiku-4-5-20251001".into());
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("llm_enriched"), "llm_enriched should be present");
        assert!(json.contains("llm_model"), "llm_model should be present");
        assert!(json.contains("claude-haiku"), "model name should be present");
    }
}
