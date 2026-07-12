use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{bail, Result};
use cih_core::Node;
use cih_wiki::{
    ClassCacheEntry, ClassEnrichmentStore, CommunityLlmSummary, ControllerLlmSummary, WikiGraph,
};
use rayon::prelude::*;

use super::config::llm_cache_key;
use crate::llm::{backoff_ms, LlmAdapter, LlmRequest};
use crate::ui::PhaseProgress;

#[allow(clippy::too_many_arguments, clippy::type_complexity)] // LLM-enrichment context bundle; refactor tracked with wiki rework
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
    pool: &rayon::ThreadPool,
) -> Result<(
    HashMap<String, ControllerLlmSummary>,
    HashMap<String, CommunityLlmSummary>,
    ClassEnrichmentStore,
)> {
    // Shared, LLM-free traversal (also used by the live-serving read-only path).
    let class_methods = cih_wiki::class_method_chains(wiki_graph, filter_route);

    let total = class_methods.len();
    tracing::info!(
        classes = total,
        "class-traversal: enriching {} unique classes",
        total
    );

    let node_by_id: HashMap<&str, &Node> = all_nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    let prev_entries = prev_store.entries.clone();

    let ui = std::sync::Arc::new(std::sync::Mutex::new(PhaseProgress::new()));
    {
        let mut locked = ui.lock().expect("UI progress mutex poisoned");
        if json_output {
            locked.hide();
        }
        locked.start_phase("Enriching classes", Some(total as u64));
    }

    let class_list: Vec<(&String, &Vec<String>)> = class_methods.iter().collect();

    let new_entries: Vec<(String, ClassCacheEntry)> = {
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
                    let content_hash = llm_cache_key(&combined, model, language);

                    if let Some(cached) = prev_entries.get(fqcn.as_str()) {
                        if cached.content_hash == content_hash {
                            ui.lock()
                                .expect("UI progress mutex poisoned")
                                .tick_skipped(format!("{} (cached)", simple_name));
                            return None;
                        }
                    }

                    ui.lock()
                        .expect("UI progress mutex poisoned")
                        .tick(simple_name);

                    let entry = if dry_run {
                        println!("--- [dry-run] class: {} ---", fqcn);
                        ClassCacheEntry {
                            content_hash,
                            method_descriptions: method_ids
                                .iter()
                                .filter_map(|id| {
                                    let m =
                                        id.split('#').nth(1).and_then(|x| x.split('/').next())?;
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
                                        std::thread::sleep(std::time::Duration::from_millis(delay));
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
                        // Do not cache failures — return None so the class is retried
                        // on the next incremental run rather than permanently poisoning
                        // the cache with an empty entry.
                        ok?
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

    ui.lock()
        .expect("UI progress mutex poisoned")
        .finish_phase();

    let store = ClassEnrichmentStore {
        schema_version: 1,
        entries: updated_entries,
    };
    // Shared, LLM-free aggregation (also used by the live-serving read-only path).
    let (ctrl_map, comm_map) = cih_wiki::build_class_maps(wiki_graph, &class_methods, &store);

    Ok((ctrl_map, comm_map, store))
}

fn build_class_system_prompt(language: &str) -> String {
    crate::llm::prompts::class_system(language)
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
        let truncated = truncate_utf8(body, 600);
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
            Some('\\') => match chars.next() {
                Some('n') => summary.push('\n'),
                Some('t') => summary.push('\t'),
                Some(c) => summary.push(c),
                None => break,
            },
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

    let extract = |val: &serde_json::Value| -> Option<(String, HashMap<String, String>)> {
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
        tracing::debug!("class enrichment: partial JSON recovered (summary only), methods lost");
        return Ok((summary, HashMap::new()));
    }
    bail!(
        "failed to extract class JSON from LLM response: {:?}",
        &text[..text.len().min(200)]
    )
}

/// Truncate `s` to at most `max_bytes` bytes without splitting a UTF-8 char.
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
