use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Result};
use cih_wiki::{FeatureLlmSummary, WikiGraph};
use cih_wiki::features::FeatureGroup;

use crate::llm::evidence::{build_evidence_pack, EvidenceCorpus};
use crate::llm::{backoff_ms, LlmAdapter, LlmRequest};

pub fn retain_matching_feature_groups(
    feature_groups: &mut Vec<FeatureGroup>,
    filter_feature: &[String],
) {
    if filter_feature.is_empty() {
        return;
    }
    let filters_lower: Vec<String> = filter_feature.iter().map(|f| f.to_lowercase()).collect();
    feature_groups.retain(|group| {
        let name = group.feature.to_lowercase();
        filters_lower.iter().any(|filter| name.contains(filter))
    });
}

pub fn build_feature_evidence(
    community_ids: &[String],
    graph: &WikiGraph,
    repo: &Path,
    corpus: &EvidenceCorpus,
) -> String {
    const MAX_FEATURE_EVIDENCE: usize = 6_000;
    let mut seen_texts = std::collections::BTreeSet::new();
    let mut merged = String::new();

    for (community_idx, comm_id) in community_ids.iter().enumerate() {
        let Some(comm_node) = graph.nodes_by_id.get(comm_id) else {
            continue;
        };
        let pack = build_evidence_pack(Some(repo), graph, comm_node, corpus);
        if pack.items.is_empty() {
            continue;
        }
        let community_evidence_id = format!("C{}", community_idx + 1);
        let section_header = format!("# {community_evidence_id} Community: {}\n", comm_node.name);
        let mut section = String::new();
        for item in &pack.items {
            if seen_texts.contains(&item.text) {
                continue;
            }
            seen_texts.insert(item.text.clone());
            section.push_str(&format!(
                "[{}-{}] {}\n",
                community_evidence_id,
                item.id,
                item.text.trim()
            ));
        }
        if section.is_empty() {
            continue;
        }
        if merged.len() + section_header.len() + section.len() > MAX_FEATURE_EVIDENCE {
            break;
        }
        merged.push_str(&section_header);
        merged.push_str(&section);
        merged.push('\n');
    }
    merged
}

pub fn enrich_one_feature(
    feature: &str,
    evidence: &str,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
    debug_evidence: bool,
    dry_run: bool,
) -> Result<FeatureLlmSummary> {
    let evidence_str = if evidence.trim().is_empty() {
        "none"
    } else {
        evidence
    };
    let system = build_feature_system_prompt();
    let user = build_feature_user_prompt(feature, evidence_str);

    if debug_evidence {
        println!("--- [feature evidence] feature: {} ---", feature);
        println!("{}", evidence_str);
        return Ok(FeatureLlmSummary {
            po_overview: format!("[debug-evidence] {}", feature),
            po_capabilities: String::new(),
            ba_process_overview: String::new(),
            ba_business_rules: String::new(),
        });
    }

    if dry_run {
        println!("--- [dry-run] feature: {} ---", feature);
        println!("System:\n{}\n", system);
        println!("User:\n{}", user);
        return Ok(FeatureLlmSummary {
            po_overview: format!("[dry-run] {}", feature),
            po_capabilities: String::new(),
            ba_process_overview: String::new(),
            ba_business_rules: String::new(),
        });
    }

    let req = LlmRequest {
        system,
        user,
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };
    let jitter_seed: u64 = feature
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let mut last_err = None;
    for attempt in 0..=(retries as usize) {
        match adapter
            .call(api_key, &req)
            .and_then(|r| parse_feature_summary(&r.text))
        {
            Ok(summary) => return Ok(summary),
            Err(err) => {
                if attempt < retries as usize {
                    let delay = backoff_ms(attempt, jitter_seed.wrapping_add(attempt as u64));
                    tracing::debug!(attempt = attempt + 1, delay_ms = delay, error = %err, "feature LLM call failed, retrying");
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                    last_err = Some(err);
                } else {
                    return Err(err);
                }
            }
        }
    }
    let _ = last_err;
    unreachable!("retry loop always returns on the final attempt")
}

fn build_feature_system_prompt() -> String {
    "You are a software architect writing business documentation from code evidence.\n\
     Write only from the provided evidence. Cite evidence IDs exactly as shown, like [C1-R1],[C1-P1],[C2-B1].\n\
     Do not invent behavior not in the evidence."
        .to_string()
}

pub fn build_feature_user_prompt(feature: &str, evidence: &str) -> String {
    format!(
        r#"You are writing feature-level documentation for the "{feature}" module.

Evidence (grouped by community):
{evidence}

Respond ONLY with a JSON object:
{{
  "po_overview": "<3-5 sentences of plain-language business overview>",
  "po_capabilities": "<bullet list of business capabilities, one per line starting with - >",
  "ba_process_overview": "<3-5 sentences describing business processes and flows>",
  "ba_business_rules": "<key business rules or invariants, one per line starting with - >"
}}"#
    )
}

pub fn parse_feature_summary(text: &str) -> Result<FeatureLlmSummary> {
    let stripped = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let json_str = if let (Some(start), Some(end)) = (stripped.find('{'), stripped.rfind('}')) {
        if start <= end {
            &stripped[start..=end]
        } else {
            stripped
        }
    } else {
        stripped
    };
    let val: serde_json::Value = serde_json::from_str(json_str).map_err(|e| {
        anyhow::anyhow!(
            "failed to parse feature LLM response: {e}: {:?}",
            &text[..text.len().min(200)]
        )
    })?;
    let summary = FeatureLlmSummary {
        po_overview: val["po_overview"].as_str().unwrap_or("").to_string(),
        po_capabilities: val["po_capabilities"].as_str().unwrap_or("").to_string(),
        ba_process_overview: val["ba_process_overview"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        ba_business_rules: val["ba_business_rules"].as_str().unwrap_or("").to_string(),
    };
    if summary.po_overview.is_empty()
        && summary.po_capabilities.is_empty()
        && summary.ba_process_overview.is_empty()
        && summary.ba_business_rules.is_empty()
    {
        bail!("feature LLM response did not contain any expected fields");
    }
    Ok(summary)
}

pub fn cached_feature_summary(
    feature: &str,
    ev_hash: &str,
    meta: Option<&cih_wiki::WikiMeta>,
) -> Option<FeatureLlmSummary> {
    let entry = meta?.feature_cache.get(feature)?;
    if entry.ev_hash != ev_hash {
        return None;
    }
    Some(FeatureLlmSummary {
        po_overview: entry.po_overview.clone(),
        po_capabilities: entry.po_capabilities.clone(),
        ba_process_overview: entry.ba_process_overview.clone(),
        ba_business_rules: entry.ba_business_rules.clone(),
    })
}

pub(super) fn resolve_feature_citations(
    summary: &mut FeatureLlmSummary,
    citation_map: &HashMap<String, String>,
) {
    if citation_map.is_empty() {
        return;
    }
    summary.po_overview = replace_citations(&summary.po_overview, citation_map);
    summary.po_capabilities = replace_citations(&summary.po_capabilities, citation_map);
    summary.ba_process_overview = replace_citations(&summary.ba_process_overview, citation_map);
    summary.ba_business_rules = replace_citations(&summary.ba_business_rules, citation_map);
}

pub(super) fn build_feature_citation_map(
    community_ids: &[String],
    graph: &WikiGraph,
    repo: &Path,
    corpus: &EvidenceCorpus,
    file_dev_map: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut citation_map: HashMap<String, String> = HashMap::new();
    let mut seen_texts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for (community_idx, comm_id) in community_ids.iter().enumerate() {
        let Some(comm_node) = graph.nodes_by_id.get(comm_id) else {
            continue;
        };
        let pack = build_evidence_pack(Some(repo), graph, comm_node, corpus);
        let community_evidence_id = format!("C{}", community_idx + 1);

        for item in &pack.items {
            if seen_texts.contains(&item.text) {
                continue;
            }
            seen_texts.insert(item.text.clone());

            if let Some(file) = item.snippet_file() {
                if let Some(url) = file_dev_map.get(file) {
                    let citation_id = format!("{}-{}", community_evidence_id, item.id);
                    citation_map.insert(citation_id, url.clone());
                }
            }
        }
    }
    citation_map
}

fn replace_citations(text: &str, map: &HashMap<String, String>) -> String {
    if map.is_empty() || !text.contains('[') {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (citation_id, url) in map {
        let bare = format!("[{}]", citation_id);
        let linked = format!("[{}]({})", citation_id, url);
        let mut pos = 0;
        while let Some(idx) = out[pos..].find(&bare) {
            let abs = pos + idx;
            let after = abs + bare.len();
            let next = out.as_bytes().get(after).copied();
            if next == Some(b'(') {
                pos = after;
            } else if next.map(|c| c.is_ascii_alphanumeric()).unwrap_or(false) {
                pos = after;
            } else {
                out.replace_range(abs..after, &linked);
                pos = abs + linked.len();
            }
        }
    }
    out
}
