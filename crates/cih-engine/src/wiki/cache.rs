use std::path::Path;

use anyhow::{Context, Result};
use cih_wiki::{
    CommunityFullCacheEntry, CommunityLlmFull, CommunityLlmSummary, FeatureLlmSummary,
    FeatureMetaEntry, FlowCacheEntry, FlowLlmSummary, WikiMeta, WikiModuleCacheEntry,
};

pub(super) fn persist_wiki_meta_caches(
    out_dir: &Path,
    community_updates: &[(String, String, CommunityLlmSummary)],
    feature_updates: &[(String, String, FeatureLlmSummary)],
    flow_updates: &[(String, String, FlowLlmSummary)],
    full_updates: &[(String, String, CommunityLlmFull)],
) -> Result<()> {
    if community_updates.is_empty()
        && feature_updates.is_empty()
        && flow_updates.is_empty()
        && full_updates.is_empty()
    {
        return Ok(());
    }

    let meta_path = out_dir.join("wiki_meta.json");
    let text = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("failed to read {}", meta_path.display()))?;
    let mut meta: WikiMeta = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", meta_path.display()))?;

    for (id, hash, summary) in community_updates {
        let entry = meta
            .module_cache
            .entry(id.clone())
            .or_insert_with(|| WikiModuleCacheEntry {
                content_hash: String::new(),
                evidence_hash: String::new(),
                page_paths: Vec::new(),
                llm_po: None,
                llm_ba: None,
                llm_dev: None,
            });
        entry.evidence_hash = hash.clone();
        entry.llm_po = Some(summary.po.clone());
        entry.llm_ba = Some(summary.ba.clone());
        entry.llm_dev = Some(summary.dev.clone());
    }

    for (feature_name, hash, summary) in feature_updates {
        meta.feature_cache.insert(
            feature_name.clone(),
            FeatureMetaEntry {
                ev_hash: hash.clone(),
                po_overview: summary.po_overview.clone(),
                po_capabilities: summary.po_capabilities.clone(),
                ba_process_overview: summary.ba_process_overview.clone(),
                ba_business_rules: summary.ba_business_rules.clone(),
            },
        );
    }

    for (handler_id, ev_hash, summary) in flow_updates {
        meta.flow_cache.insert(
            handler_id.clone(),
            FlowCacheEntry {
                evidence_hash: ev_hash.clone(),
                summary: summary.clone(),
            },
        );
    }

    for (comm_id, ev_hash, full) in full_updates {
        meta.full_cache.insert(
            comm_id.clone(),
            CommunityFullCacheEntry {
                evidence_hash: ev_hash.clone(),
                po_summary: full.po_summary.clone(),
                po_capabilities: full.po_capabilities.clone(),
                po_workflows: full.po_workflows.clone(),
                po_open_questions: full.po_open_questions.clone(),
                ba_process_overview: full.ba_process_overview.clone(),
                ba_contracts: full.ba_contracts.clone(),
                ba_business_rules: full.ba_business_rules.clone(),
                dev_responsibility: full.dev_responsibility.clone(),
                dev_key_classes: full.dev_key_classes.clone(),
                dev_entry_points: full.dev_entry_points.clone(),
            },
        );
    }

    let json = serde_json::to_string_pretty(&meta).context("failed to serialize wiki metadata")?;
    std::fs::write(&meta_path, json)
        .with_context(|| format!("failed to write {}", meta_path.display()))?;

    Ok(())
}
