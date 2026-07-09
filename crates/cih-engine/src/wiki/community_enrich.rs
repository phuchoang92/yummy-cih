use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Result};
use cih_core::Node;
use cih_wiki::{CommunityLlmFull, FlowLlmSummary, WikiGraph};
use rayon::prelude::*;

use super::config::LlmRunParams;
use super::flow_enrich::enrich_one_flow;
use crate::llm::evidence::{build_evidence_pack, EvidenceCorpus};
use crate::llm::{backoff_ms, LlmAdapter, LlmRequest};
use crate::ui::PhaseProgress;

pub(super) fn run_community_full_enrichment(
    community_nodes: &[Node],
    graph: &WikiGraph,
    repo: &Path,
    evidence_corpus: &EvidenceCorpus,
    pool: &rayon::ThreadPool,
    llm: &LlmRunParams<'_>,
    json: bool,
) -> Option<HashMap<String, CommunityLlmFull>> {
    let total_full = community_nodes.len();
    tracing::info!(communities = total_full, "starting LLM full enrichment");

    let mut ui_full = PhaseProgress::new();
    if json {
        ui_full.hide();
    }
    ui_full.start_phase("Deep enrichment (PO/BA)", Some(total_full as u64));

    let results: Vec<(String, Result<CommunityLlmFull>)> = pool.install(|| {
        community_nodes
            .par_iter()
            .map(|comm| {
                ui_full.tick(comm.name.as_str());
                let r = enrich_one_community_full(
                    comm,
                    graph,
                    repo,
                    evidence_corpus,
                    llm.adapter,
                    llm.api_key,
                    llm.model,
                    llm.max_tokens,
                    llm.timeout_secs,
                    llm.retries,
                    llm.language,
                );
                if r.is_ok() {
                    ui_full.inc_ok();
                } else {
                    ui_full.inc_failed();
                }
                (comm.id.as_str().to_string(), r)
            })
            .collect()
    });
    ui_full.finish_phase();

    let mut map = HashMap::new();
    for (id, result) in results {
        match result {
            Ok(full) => {
                map.insert(id, full);
            }
            Err(err) => tracing::warn!(community = %id, error = %err, "LLM full enrichment failed"),
        }
    }
    tracing::info!(enriched = map.len(), "LLM full enrichment complete");
    if map.is_empty() {
        None
    } else {
        Some(map)
    }
}

pub(super) fn run_process_flow_enrichment(
    graph: &WikiGraph,
    llm: &LlmRunParams<'_>,
    json: bool,
) -> HashMap<String, FlowLlmSummary> {
    let total_flows = graph.process_nodes.len();
    if total_flows == 0 {
        return HashMap::new();
    }
    tracing::info!(flows = total_flows, "starting per-flow LLM enrichment");
    let mut ui_flow = PhaseProgress::new();
    if json {
        ui_flow.hide();
    }
    ui_flow.start_phase("Enriching flows", Some(total_flows as u64));
    let mut map = HashMap::new();
    for proc in &graph.process_nodes {
        ui_flow.tick(proc.name.as_str());
        match enrich_one_flow(
            proc,
            graph,
            llm.adapter,
            llm.api_key,
            llm.model,
            llm.max_tokens,
            llm.timeout_secs,
            llm.retries,
            llm.language,
            llm.debug_evidence,
            llm.dry_run,
        ) {
            Ok(summary) => {
                map.insert(proc.id.as_str().to_string(), summary);
                ui_flow.inc_ok();
            }
            Err(err) => {
                tracing::warn!(flow = %proc.id, error = %err, "flow LLM enrichment failed");
                ui_flow.inc_failed();
            }
        }
    }
    ui_flow.finish_phase();
    tracing::info!(enriched = map.len(), "per-flow LLM enrichment complete");
    map
}

fn build_full_prompt(name: &str, evidence: &str) -> String {
    let evidence = if evidence.trim().is_empty() {
        "none"
    } else {
        evidence
    };
    format!(
        "You are writing detailed documentation from a code analysis graph.\nModule: \"{name}\"\n\nEvidence:\n{evidence}\n\n{}",
        crate::llm::prompts::COMMUNITY_FULL_JSON_TEMPLATE
    )
}

fn parse_llm_full(text: &str) -> Result<CommunityLlmFull> {
    let try_extract = |val: &serde_json::Value| -> Option<CommunityLlmFull> {
        let f = |key: &str| val[key].as_str().unwrap_or("").to_string();
        let full = CommunityLlmFull {
            po_summary: f("po_summary"),
            po_capabilities: f("po_capabilities"),
            po_workflows: f("po_workflows"),
            po_open_questions: f("po_open_questions"),
            ba_process_overview: f("ba_process_overview"),
            ba_contracts: f("ba_contracts"),
            ba_business_rules: f("ba_business_rules"),
            dev_responsibility: f("dev_responsibility"),
            dev_key_classes: f("dev_key_classes"),
            dev_entry_points: f("dev_entry_points"),
        };
        if full.po_summary.is_empty() && full.ba_process_overview.is_empty() {
            None
        } else {
            Some(full)
        }
    };
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(text.trim()) {
        if let Some(r) = try_extract(&val) {
            return Ok(r);
        }
    }
    if let (Some(s), Some(e)) = (text.find('{'), text.rfind('}')) {
        if s < e {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text[s..=e]) {
                if let Some(r) = try_extract(&val) {
                    return Ok(r);
                }
            }
        }
    }
    bail!(
        "failed to extract llm-full JSON from response: {:?}",
        &text[..text.len().min(200)]
    )
}

#[allow(clippy::too_many_arguments)] // LLM-enrichment context bundle; refactor tracked with wiki rework
fn enrich_one_community_full(
    community: &cih_core::Node,
    graph: &WikiGraph,
    repo: &Path,
    evidence_corpus: &EvidenceCorpus,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
    language: &str,
) -> Result<CommunityLlmFull> {
    let evidence_pack = build_evidence_pack(Some(repo), graph, community, evidence_corpus);
    let evidence = evidence_pack.render();
    let system = crate::llm::prompts::community_system(language);
    let user = build_full_prompt(&community.name, &evidence);
    let request = LlmRequest {
        system,
        user,
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };
    let jitter: u64 = community
        .id
        .as_str()
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let mut last_err = None;
    for attempt in 0..=(retries as usize) {
        match adapter
            .call(api_key, &request)
            .and_then(|r| parse_llm_full(&r.text))
        {
            Ok(full) => return Ok(full),
            Err(err) => {
                if attempt < retries as usize {
                    let delay = backoff_ms(attempt, jitter.wrapping_add(attempt as u64));
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
