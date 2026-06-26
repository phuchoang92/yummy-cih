use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{bail, Result};
use cih_core::Node;
use cih_wiki::{
    ClassCacheEntry, ClassEnrichmentStore, CommunityLlmSummary, ControllerLlmSummary, WikiGraph,
};
use rayon::prelude::*;

use crate::llm::{backoff_ms, LlmAdapter, LlmRequest};
use crate::ui::PhaseProgress;
use super::config::fnv64;

pub fn enrich_classes_for_chains(
    wiki_graph: &WikiGraph,
    all_nodes: &[Node],
    repo: &Path,
    prev_store: ClassEnrichmentStore,
    adapter: &dyn LlmAdapter,
    api_key: Option<&str>,
    model: &str,
    max_tokens: u32,
    timeout_secs: u64,
    retries: u32,
    language: &str,
    dry_run: bool,
    json_output: bool,
    filter_route: &[String],
    concurrency: usize,
) -> Result<(
    HashMap<String, ControllerLlmSummary>,
    HashMap<String, CommunityLlmSummary>,
    ClassEnrichmentStore,
)> {
    let mut class_methods: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for routes in wiki_graph.routes_by_controller.values() {
        for (handler, route) in routes {
            if !filter_route.is_empty() && {
                let path = cih_wiki::graph::route_path(route);
                !filter_route.iter().any(|f| path.contains(f.as_str()))
            }
            {
                continue;
            }
            let chain = wiki_graph.build_call_chain(handler.id.as_str(), 4);
            for method_id in chain {
                let fqcn = method_id
                    .strip_prefix("Method:")
                    .or_else(|| method_id.strip_prefix("Constructor:"))
                    .and_then(|s| s.split('#').next())
                    .unwrap_or("")
                    .to_string();
                if fqcn.is_empty() {
                    continue;
                }
                let methods = class_methods.entry(fqcn).or_default();
                if !methods.contains(&method_id) {
                    methods.push(method_id);
                }
            }
        }
    }

    let total = class_methods.len();
    tracing::info!(classes = total, "class-traversal: enriching {} unique classes", total);

    let node_by_id: HashMap<&str, &Node> =
        all_nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    let prev_entries = prev_store.entries.clone();

    let ui = std::sync::Arc::new(std::sync::Mutex::new(PhaseProgress::new()));
    {
        let mut locked = ui.lock().expect("UI progress mutex poisoned");
        if json_output {
            locked.hide();
        }
        locked.start_phase("Enriching classes", Some(total as u64));
    }

    let effective_concurrency = concurrency.max(1);
    let class_list: Vec<(&String, &Vec<String>)> = class_methods.iter().collect();

    let new_entries: Vec<(String, ClassCacheEntry)> = {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(effective_concurrency)
            .build()
            .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().expect("failed to build default rayon thread pool"));

        pool.install(|| {
            class_list
                .par_iter()
                .filter_map(|(fqcn, method_ids)| {
                    let simple_name = fqcn.rsplit('.').next().unwrap_or(fqcn.as_str());

                    let method_nodes: Vec<Node> = method_ids
                        .iter()
                        .filter_map(|id| node_by_id.get(id.as_str()).copied().cloned())
                        .collect();

                    let bodies = cih_wiki::source_bodies(&method_nodes, repo);

                    let mut sorted_bodies: Vec<(&str, &str)> = method_ids
                        .iter()
                        .filter_map(|id| {
                            bodies
                                .get(id.as_str())
                                .map(|b| (id.as_str(), b.stripped.as_str()))
                        })
                        .collect();
                    sorted_bodies.sort_by_key(|(id, _)| *id);

                    let combined = sorted_bodies
                        .iter()
                        .map(|(_, b)| *b)
                        .collect::<Vec<_>>()
                        .join("\n---\n");
                    let content_hash = fnv64(&combined);

                    if let Some(cached) = prev_entries.get(fqcn.as_str()) {
                        if cached.content_hash == content_hash {
                            ui.lock().expect("UI progress mutex poisoned").tick_skipped(format!("{} (cached)", simple_name));
                            return None;
                        }
                    }

                    ui.lock().expect("UI progress mutex poisoned").tick(simple_name);

                    let entry = if dry_run {
                        println!("--- [dry-run] class: {} ---", fqcn);
                        ClassCacheEntry {
                            content_hash,
                            method_descriptions: method_ids
                                .iter()
                                .filter_map(|id| {
                                    let m = id
                                        .split('#')
                                        .nth(1)
                                        .and_then(|x| x.split('/').next())?;
                                    Some((m.to_string(), format!("[dry-run] {}", m)))
                                })
                                .collect(),
                            class_summary: format!("[dry-run] {}", simple_name),
                        }
                    } else {
                        let system = build_class_system_prompt(language);
                        let user = build_class_enrich_prompt(fqcn, &sorted_bodies);
                        let request = LlmRequest {
                            system,
                            user,
                            model: model.to_string(),
                            max_tokens: max_tokens.max(2000),
                            timeout_secs,
                        };
                        let jitter: u64 = fqcn
                            .bytes()
                            .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
                        let mut ok = None;
                        for attempt in 0..=(retries as usize) {
                            match adapter
                                .call(api_key, &request)
                                .and_then(|r| parse_class_enrich_response(&r.text))
                            {
                                Ok((summary, method_descs)) => {
                                    ok = Some(ClassCacheEntry {
                                        content_hash: content_hash.clone(),
                                        method_descriptions: method_descs,
                                        class_summary: summary,
                                    });
                                    break;
                                }
                                Err(err) => {
                                    if attempt < retries as usize {
                                        let delay = backoff_ms(
                                            attempt,
                                            jitter.wrapping_add(attempt as u64),
                                        );
                                        std::thread::sleep(std::time::Duration::from_millis(
                                            delay,
                                        ));
                                    } else {
                                        tracing::warn!(
                                            class = %fqcn,
                                            error = %err,
                                            "class enrichment failed"
                                        );
                                    }
                                }
                            }
                        }
                        ok.unwrap_or_else(|| ClassCacheEntry {
                            content_hash,
                            method_descriptions: HashMap::new(),
                            class_summary: String::new(),
                        })
                    };

                    Some(((*fqcn).clone(), entry))
                })
                .collect()
        })
    };

    let mut updated_entries: BTreeMap<String, ClassCacheEntry> = prev_entries;
    for (fqcn, entry) in new_entries {
        updated_entries.insert(fqcn, entry);
    }

    ui.lock().expect("UI progress mutex poisoned").finish_phase();

    let mut ctrl_map: HashMap<String, ControllerLlmSummary> = HashMap::new();
    for (fqcn, _) in &class_methods {
        let simple_name = fqcn.rsplit('.').next().unwrap_or(fqcn.as_str()).to_string();
        if let Some(entry) = updated_entries.get(fqcn.as_str()) {
            ctrl_map.insert(
                simple_name,
                ControllerLlmSummary {
                    description: entry.class_summary.clone(),
                    feature: None,
                    method_descriptions: entry.method_descriptions.clone(),
                },
            );
        }
    }

    let mut comm_texts: HashMap<String, Vec<String>> = HashMap::new();
    for (fqcn, method_ids) in &class_methods {
        let Some(entry) = updated_entries.get(fqcn.as_str()) else {
            continue;
        };
        if entry.class_summary.is_empty() {
            continue;
        }
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for mid in method_ids {
            if let Some(comm_id) = wiki_graph.community_by_member.get(mid.as_str()) {
                if seen.insert(comm_id.as_str()) {
                    comm_texts
                        .entry(comm_id.clone())
                        .or_default()
                        .push(entry.class_summary.clone());
                }
            }
        }
    }
    let comm_map: HashMap<String, CommunityLlmSummary> = comm_texts
        .into_iter()
        .map(|(id, summaries)| {
            let text = summaries.join(" ");
            (
                id,
                CommunityLlmSummary {
                    po: text.clone(),
                    ba: text,
                    dev: String::new(),
                },
            )
        })
        .collect();

    Ok((
        ctrl_map,
        comm_map,
        ClassEnrichmentStore {
            schema_version: 1,
            entries: updated_entries,
        },
    ))
}

fn build_class_system_prompt(language: &str) -> String {
    let mut s = String::from(
        "You are a code documentation assistant. Describe Java class methods in one sentence \
         each for a business analyst. Return JSON only. Do not invent behavior. \
         Start each method description with an action verb. \
         Do not mention the class name, method name, or arity (e.g. /2()) in the description.",
    );
    if language != "en" {
        s.push_str(&format!(" Write all descriptions in language: {}.", language));
    }
    s
}

fn build_class_enrich_prompt(fqcn: &str, bodies: &[(&str, &str)]) -> String {
    let simple = fqcn.rsplit('.').next().unwrap_or(fqcn);
    let mut s = format!("Class: {simple}\n\nMethods:\n");
    for (i, (method_id, body)) in bodies.iter().enumerate() {
        let method_name = method_id
            .split('#')
            .nth(1)
            .and_then(|x| x.split('/').next())
            .unwrap_or("unknown");
        let truncated = if body.len() > 600 { &body[..600] } else { body };
        s.push_str(&format!("{}. {}\n{}\n\n", i + 1, method_name, truncated));
    }
    s.push_str(
        "Return exactly this JSON:\n\
         {\n\
           \"summary\": \"one paragraph: what this class does in the system\",\n\
           \"methods\": {\n\
             \"methodName\": \"Validates the request payload and delegates to the write service.\"\n\
           }\n\
         }\n\
         Each method value must start with a verb and must not repeat the class or method name.\n\
         Output only the JSON object.",
    );
    s
}

fn extract_summary_from_partial(text: &str) -> Option<String> {
    let key = "\"summary\":";
    let start = text.find(key)?;
    let after_key = text[start + key.len()..].trim_start();
    if !after_key.starts_with('"') {
        return None;
    }
    let s = &after_key[1..];
    let mut summary = String::new();
    let mut chars = s.chars().peekable();
    loop {
        match chars.next() {
            Some('\\') => {
                match chars.next() {
                    Some('n') => summary.push('\n'),
                    Some('t') => summary.push('\t'),
                    Some(c) => summary.push(c),
                    None => break,
                }
            }
            Some('"') => break,
            Some(c) => summary.push(c),
            None => break,
        }
    }
    if summary.trim().is_empty() {
        None
    } else {
        Some(summary.trim().to_string())
    }
}

fn parse_class_enrich_response(text: &str) -> Result<(String, HashMap<String, String>)> {
    let cleaned = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let extract =
        |val: &serde_json::Value| -> Option<(String, HashMap<String, String>)> {
            let summary = val["summary"].as_str().unwrap_or("").to_string();
            let methods: HashMap<String, String> = val["methods"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            if summary.is_empty() && methods.is_empty() {
                None
            } else {
                Some((summary, methods))
            }
        };

    if let Ok(val) = serde_json::from_str::<serde_json::Value>(cleaned) {
        if let Some(r) = extract(&val) {
            return Ok(r);
        }
    }
    if let (Some(s), Some(e)) = (cleaned.find('{'), cleaned.rfind('}')) {
        if s < e {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&cleaned[s..=e]) {
                if let Some(r) = extract(&val) {
                    return Ok(r);
                }
            }
        }
    }
    if let Some(summary) = extract_summary_from_partial(cleaned) {
        tracing::debug!(
            "class enrichment: partial JSON recovered (summary only), methods lost"
        );
        return Ok((summary, HashMap::new()));
    }
    bail!(
        "failed to extract class JSON from LLM response: {:?}",
        &text[..text.len().min(200)]
    )
}
