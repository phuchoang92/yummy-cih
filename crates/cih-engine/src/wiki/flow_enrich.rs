use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Result};
use cih_core::Node;
use cih_wiki::{FlowCacheEntry, FlowLlmSummary, WikiGraph};
use rayon::prelude::*;

use crate::llm::{backoff_ms, LlmAdapter, LlmRequest};
use crate::ui::PhaseProgress;
use super::config::fnv64;

fn build_flow_evidence(process_node: &Node, graph: &WikiGraph) -> String {
    const MAX_FLOW_EVIDENCE: usize = 2_000;
    let proc_id = process_node.id.as_str();
    let mut out = String::new();

    if let Some(route) = process_node
        .props
        .as_ref()
        .and_then(|p| p.get("route"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        out.push_str(&format!("Triggered by: {}\n\n", route));
    }

    let Some(steps) = graph.process_steps.get(proc_id) else {
        return out;
    };

    out.push_str("Steps:\n");
    for step in steps {
        let method_id = step.symbol.id.as_str();

        let class_name = method_id
            .split_once('#')
            .map(|(prefix, _)| {
                prefix
                    .trim_start_matches(crate::node_prefix::METHOD)
                    .trim_start_matches(crate::node_prefix::CONSTRUCTOR)
                    .rsplit('.')
                    .next()
                    .unwrap_or(prefix)
            })
            .unwrap_or("");

        let stereotype = method_id
            .split_once('#')
            .and_then(|(prefix, _)| {
                let fqcn = prefix
                    .trim_start_matches(crate::node_prefix::METHOD)
                    .trim_start_matches(crate::node_prefix::CONSTRUCTOR);
                let cls_id = format!("{}{}", crate::node_prefix::CLASS, fqcn);
                graph
                    .nodes_by_id
                    .get(&cls_id)
                    .and_then(cih_wiki::graph::node_stereotype)
            })
            .unwrap_or("");

        let empty_calls: Vec<String> = Vec::new();
        let calls: Vec<&str> = graph
            .calls_out
            .get(method_id)
            .unwrap_or(&empty_calls)
            .iter()
            .take(4)
            .filter_map(|cid| graph.nodes_by_id.get(cid).map(|n| n.name.as_str()))
            .collect();

        let mut tables: Vec<String> = Vec::new();
        if let Some(qids) = graph.executes_query.get(method_id) {
            for qid in qids.iter().take(4) {
                for tid in graph
                    .query_reads_table
                    .get(qid.as_str())
                    .into_iter()
                    .flatten()
                    .take(2)
                {
                    let name = tid.strip_prefix(crate::node_prefix::DB_TABLE).unwrap_or(tid);
                    tables.push(format!("{}(r)", name));
                }
                for tid in graph
                    .query_writes_table
                    .get(qid.as_str())
                    .into_iter()
                    .flatten()
                    .take(2)
                {
                    let name = tid.strip_prefix(crate::node_prefix::DB_TABLE).unwrap_or(tid);
                    tables.push(format!("{}(w)", name));
                }
            }
        }

        let mut line = format!(
            "[{}] {} — {} ({})",
            step.step_number,
            step.symbol.name,
            class_name,
            if stereotype.is_empty() { "?" } else { stereotype }
        );
        if !calls.is_empty() {
            line.push_str(&format!(" | calls: {}", calls.join(", ")));
        }
        if !tables.is_empty() {
            line.push_str(&format!(" | tables: {}", tables.join(", ")));
        }
        line.push('\n');

        if out.len() + line.len() > MAX_FLOW_EVIDENCE {
            break;
        }
        out.push_str(&line);
    }

    out
}

fn chain_steps_text(chain: &[String], graph: &WikiGraph) -> String {
    chain
        .iter()
        .enumerate()
        .map(|(i, mid)| {
            let (class_name, method_name) = mid
                .split_once('#')
                .map(|(prefix, method)| {
                    let cls = prefix
                        .trim_start_matches(crate::node_prefix::METHOD)
                        .trim_start_matches(crate::node_prefix::CONSTRUCTOR)
                        .rsplit('.')
                        .next()
                        .unwrap_or(prefix);
                    (cls, method)
                })
                .unwrap_or(("?", mid.as_str()));
            let cls_id = cih_wiki::pages::api_flow::class_id_from_method_id(mid.as_str(), graph);
            let stereotype = graph
                .nodes_by_id
                .get(cls_id.as_str())
                .and_then(cih_wiki::graph::node_stereotype)
                .unwrap_or("?");
            format!(
                "[{}] {}.{}() ({})",
                i + 1,
                class_name,
                method_name,
                stereotype
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)] // LLM-enrichment context bundle; refactor tracked with wiki rework
pub(super) fn enrich_route_flows(
    graph: &WikiGraph,
    scope: Option<&std::collections::HashSet<String>>,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
    language: &str,
    dry_run: bool,
    flow_cache: &BTreeMap<String, FlowCacheEntry>,
    pool: &rayon::ThreadPool,
) -> (HashMap<String, FlowLlmSummary>, Vec<(String, String, FlowLlmSummary)>) {
    let handlers: Vec<(String, String)> = graph
        .routes_by_controller
        .values()
        .flat_map(|routes| {
            routes
                .iter()
                .map(|(handler, _route)| (handler.id.as_str().to_string(), handler.name.clone()))
        })
        .filter(|(id, _)| scope.is_none_or(|s| s.contains(id.as_str())))
        .collect();

    if handlers.is_empty() {
        return (HashMap::new(), Vec::new());
    }

    let ui = std::sync::Arc::new(std::sync::Mutex::new(PhaseProgress::new()));
    ui.lock()
        .expect("UI progress mutex poisoned")
        .start_phase("Enriching route flows", Some(handlers.len() as u64));

    let raw: Vec<(String, FlowLlmSummary, Option<String>)> = pool.install(|| {
        handlers
            .par_iter()
            .filter_map(|(handler_id, handler_name)| {
                let chain = graph.build_call_chain(handler_id.as_str(), 4);
                if chain.is_empty() {
                    ui.lock().expect("UI progress mutex poisoned").inc_ok();
                    return None;
                }
                let step_count = chain.len();
                let steps_text = chain_steps_text(&chain, graph);
                let evidence_hash = fnv64(&steps_text);

                if let Some(cached) = flow_cache.get(handler_id.as_str()) {
                    if cached.evidence_hash == evidence_hash {
                        ui.lock().expect("UI progress mutex poisoned").inc_ok();
                        return Some((handler_id.clone(), cached.summary.clone(), None));
                    }
                }

                ui.lock().expect("UI progress mutex poisoned").tick(handler_name.as_str());

                if dry_run {
                    let summary = FlowLlmSummary {
                        narrative: format!("[dry-run] {}", handler_name),
                        business_impact: String::new(),
                        step_descriptions: vec!["[dry-run]".into(); step_count],
                    };
                    ui.lock().expect("UI progress mutex poisoned").inc_ok();
                    return Some((handler_id.clone(), summary, None));
                }

                let system = crate::llm::prompts::http_flow_system(language);
                let json_template = crate::llm::prompts::HTTP_FLOW_JSON_TEMPLATE
                    .replace("{step_count}", &step_count.to_string());
                let user = format!(
                    "HTTP handler: \"{}\"\n\nCall chain ({} steps):\n{}\n\n{}",
                    handler_name, step_count, steps_text, json_template,
                );
                let effective_max_tokens = route_flow_token_budget(step_count, max_tokens);
                let req = LlmRequest {
                    system,
                    user,
                    model: model.to_string(),
                    max_tokens: effective_max_tokens,
                    timeout_secs,
                };
                let jitter_seed: u64 = handler_id
                    .as_str()
                    .bytes()
                    .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));

                let mut last_err = None;
                for attempt in 0..=(retries as usize) {
                    match adapter
                        .call(api_key, &req)
                        .and_then(|r| parse_flow_summary(&r.text, step_count))
                    {
                        Ok(summary) => {
                            ui.lock().expect("UI progress mutex poisoned").inc_ok();
                            return Some((
                                handler_id.clone(),
                                summary,
                                Some(evidence_hash),
                            ));
                        }
                        Err(err) => {
                            if attempt < retries as usize {
                                let delay =
                                    backoff_ms(attempt, jitter_seed.wrapping_add(attempt as u64));
                                tracing::debug!(
                                    attempt = attempt + 1,
                                    delay_ms = delay,
                                    error = %err,
                                    "route flow LLM call failed, retrying"
                                );
                                std::thread::sleep(std::time::Duration::from_millis(delay));
                            }
                            last_err = Some(err);
                        }
                    }
                }
                tracing::warn!(
                    handler = %handler_id,
                    error = %last_err.expect("last_err always set after retry loop"),
                    "route flow LLM enrichment failed"
                );
                ui.lock().expect("UI progress mutex poisoned").inc_failed();
                None
            })
            .collect()
    });

    ui.lock().expect("UI progress mutex poisoned").finish_phase();

    let mut result = HashMap::with_capacity(raw.len());
    let mut cache_updates = Vec::new();
    for (handler_id, summary, maybe_hash) in raw {
        if let Some(ev_hash) = maybe_hash {
            cache_updates.push((handler_id.clone(), ev_hash, summary.clone()));
        }
        result.insert(handler_id, summary);
    }
    (result, cache_updates)
}

#[allow(clippy::too_many_arguments)] // LLM-enrichment context bundle; refactor tracked with wiki rework
pub(super) fn enrich_one_flow(
    process_node: &Node,
    graph: &WikiGraph,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
    language: &str,
    debug_evidence: bool,
    dry_run: bool,
) -> Result<FlowLlmSummary> {
    let evidence = build_flow_evidence(process_node, graph);
    let step_count = graph
        .process_steps
        .get(process_node.id.as_str())
        .map(|s| s.len())
        .unwrap_or(0);

    let system = crate::llm::prompts::process_flow_system(language);
    let evidence_str = if evidence.trim().is_empty() { "none" } else { &evidence };
    let json_template = crate::llm::prompts::PROCESS_FLOW_JSON_TEMPLATE
        .replace("{step_count}", &step_count.to_string());
    let user = format!(
        "Process: \"{}\"\n\n{}\n\n{}",
        process_node.name, evidence_str, json_template,
    );

    if debug_evidence {
        println!("--- [flow evidence] process: {} ---", process_node.name);
        println!("{}", evidence_str);
        return Ok(FlowLlmSummary {
            narrative: format!("[debug-evidence] {}", process_node.name),
            business_impact: String::new(),
            step_descriptions: vec!["[debug]".into(); step_count],
        });
    }
    if dry_run {
        println!("--- [dry-run] flow: {} ---", process_node.name);
        println!("System:\n{}\n", system);
        println!("User:\n{}", user);
        return Ok(FlowLlmSummary {
            narrative: format!("[dry-run] {}", process_node.name),
            business_impact: String::new(),
            step_descriptions: vec!["[dry-run]".into(); step_count],
        });
    }

    let req = LlmRequest {
        system,
        user,
        model: model.to_string(),
        max_tokens,
        timeout_secs,
    };
    let jitter_seed: u64 = process_node
        .id
        .as_str()
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    let mut last_err = None;
    for attempt in 0..=(retries as usize) {
        match adapter
            .call(api_key, &req)
            .and_then(|r| parse_flow_summary(&r.text, step_count))
        {
            Ok(summary) => return Ok(summary),
            Err(err) => {
                if attempt < retries as usize {
                    let delay = backoff_ms(attempt, jitter_seed.wrapping_add(attempt as u64));
                    tracing::debug!(attempt = attempt + 1, delay_ms = delay, error = %err, "flow LLM call failed, retrying");
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

fn route_flow_token_budget(step_count: usize, base: u32) -> u32 {
    base.max(step_count as u32 * 100 + 500).max(2000)
}

fn extract_flow_partial(text: &str, step_count: usize) -> Option<FlowLlmSummary> {
    fn extract_string_value(text: &str, key: &str) -> Option<String> {
        let needle = format!("\"{}\":", key);
        let start = text.find(needle.as_str())?;
        let after = text[start + needle.len()..].trim_start();
        if !after.starts_with('"') {
            return None;
        }
        let mut out = String::new();
        let mut chars = after[1..].chars().peekable();
        loop {
            match chars.next() {
                Some('\\') => match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(c) => out.push(c),
                    None => break,
                },
                Some('"') => break,
                Some(c) => out.push(c),
                None => break,
            }
        }
        if out.trim().is_empty() { None } else { Some(out.trim().to_string()) }
    }

    let narrative = extract_string_value(text, "narrative").unwrap_or_default();
    let business_impact = extract_string_value(text, "business_impact").unwrap_or_default();

    if narrative.is_empty() && business_impact.is_empty() {
        return None;
    }

    let mut descs = Vec::new();
    if let Some(arr_start) = text.find("\"step_descriptions\"") {
        let after = &text[arr_start..];
        if let Some(bracket) = after.find('[') {
            let content = &after[bracket + 1..];
            let mut in_str = false;
            let mut current = String::new();
            let mut chars = content.chars();
            loop {
                match chars.next() {
                    None | Some(']') => {
                        if in_str && !current.trim().is_empty() {
                            descs.push(current.trim().to_string());
                        }
                        break;
                    }
                    Some('"') if !in_str => { in_str = true; }
                    Some('"') if in_str => {
                        descs.push(current.trim().to_string());
                        current = String::new();
                        in_str = false;
                    }
                    Some('\\') if in_str => {
                        match chars.next() {
                            Some('n') => current.push('\n'),
                            Some('t') => current.push('\t'),
                            Some(c) => current.push(c),
                            None => break,
                        }
                    }
                    Some(c) if in_str => current.push(c),
                    _ => {}
                }
            }
        }
    }
    descs.resize(step_count, String::new());

    Some(FlowLlmSummary { narrative, business_impact, step_descriptions: descs })
}

pub fn parse_flow_summary(text: &str, step_count: usize) -> Result<FlowLlmSummary> {
    if text.trim().is_empty() {
        bail!("LLM returned an empty response");
    }
    let stripped = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let json_str = if let (Some(s), Some(e)) = (stripped.find('{'), stripped.rfind('}')) {
        if s <= e { &stripped[s..=e] } else { stripped }
    } else {
        stripped
    };
    match serde_json::from_str::<serde_json::Value>(json_str) {
        Ok(val) => {
            let narrative = val["narrative"].as_str().unwrap_or("").to_string();
            let business_impact = val["business_impact"].as_str().unwrap_or("").to_string();
            let mut descs: Vec<String> = val["step_descriptions"]
                .as_array()
                .map(|arr| arr.iter().map(|v| v.as_str().unwrap_or("").to_string()).collect())
                .unwrap_or_default();
            descs.resize(step_count, String::new());

            if narrative.is_empty() && business_impact.is_empty() && descs.iter().all(|s| s.is_empty()) {
                bail!("flow LLM response did not contain any expected fields");
            }
            Ok(FlowLlmSummary { narrative, business_impact, step_descriptions: descs })
        }
        Err(parse_err) => {
            if let Some(partial) = extract_flow_partial(stripped, step_count) {
                tracing::debug!("flow enrichment: partial JSON recovered (narrative/impact only)");
                return Ok(partial);
            }
            bail!(
                "failed to parse flow LLM response: {parse_err}: {:?}",
                &text[..text.len().min(200)]
            )
        }
    }
}
