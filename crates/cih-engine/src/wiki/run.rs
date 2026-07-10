use std::collections::{BTreeMap, HashMap};

use anyhow::{bail, Context, Result};
use cih_wiki::features::group_communities_by_feature;
use cih_wiki::graph::route_path;
use cih_wiki::{
    generate_wiki, ClassEnrichmentStore, CommunityFullCacheEntry, CommunityLlmFull,
    CommunityLlmSummary, ControllerLlmSummary, FeatureLlmSummary, FlowLlmSummary,
    WikiGenerationInfo, WikiInput, WikiLlmInfo, WikiModuleTree,
};

use crate::llm::evidence::EvidenceCorpus;
use crate::llm::{make_adapter, resolve_api_key, LlmProvider};

use super::cache::persist_wiki_meta_caches;
use super::class_enrich::enrich_classes_for_chains;
use super::community_enrich::{run_community_full_enrichment, run_process_flow_enrichment};
use super::config::{
    fnv64, llm_cache_key, load_class_enrichment, load_wiki_meta, save_class_enrichment,
    LlmRunParams, WikiConfig, WikiGrouping, WikiMode, PROMPT_VERSION,
};
use super::feature_enrich::{
    build_feature_citation_map, build_feature_evidence, cached_feature_summary, enrich_one_feature,
    resolve_feature_citations, retain_matching_feature_groups,
};
use super::flow_enrich::enrich_route_flows;
use super::loader::load_wiki_artifacts;

pub fn run_wiki(cfg: WikiConfig) -> Result<()> {
    let WikiConfig {
        repo,
        out,
        run_llm,
        llm:
            crate::llm::LlmCallConfig {
                provider: llm_provider,
                base_url: llm_base_url,
                model: llm_model,
                api_key_env: llm_api_key_env,
                max_tokens: llm_max_tokens,
                timeout_secs: llm_timeout_secs,
                retries: llm_retries,
            },
        llm_provider_config,
        evidence_paths,
        llm_concurrency,
        llm_debug_evidence,
        llm_dry_run,
        wiki_language,
        wiki_mode,
        grouping,
        html,
        incremental,
        save_evidence,
        filter_community,
        max_communities,
        filter_feature,
        filter_route,
        json,
        check_only,
    } = cfg;
    let repo = repo.as_path();
    let default_model = match llm_provider {
        LlmProvider::Gemini => "gemini-2.5-flash",
        LlmProvider::Anthropic => "claude-haiku-4-5-20251001",
        LlmProvider::Bedrock => "us.anthropic.claude-haiku-4-5-20251001",
        LlmProvider::DeepSeek => "deepseek-chat",
        _ => "gpt-4o-mini",
    };
    let llm_model_owned;
    let llm_model = if llm_model.is_empty() {
        default_model
    } else {
        llm_model_owned = llm_model;
        llm_model_owned.as_str()
    };
    let wiki_language = wiki_language.as_str();
    if wiki_language.is_empty() {
        bail!("--wiki-language must not be empty (e.g. en, vi, ja, fr)");
    }
    let effective_run_llm =
        run_llm || matches!(wiki_mode, WikiMode::LlmSummary | WikiMode::LlmFull);
    let llm_max_tokens = if wiki_mode == WikiMode::LlmFull {
        llm_max_tokens.max(2048)
    } else {
        llm_max_tokens
    };
    let llm_no_call = llm_dry_run || llm_debug_evidence;

    let span = tracing::info_span!("wiki", repo = %repo.display());
    let _enter = span.enter();

    tracing::info!(
        repo = %repo.display(),
        mode = %wiki_mode,
        grouping = %grouping,
        llm = effective_run_llm,
        "starting wiki"
    );

    let art = match load_wiki_artifacts(
        repo,
        out,
        grouping,
        &filter_community,
        max_communities,
        &filter_route,
    )? {
        Some(a) => a,
        None => return Ok(()),
    };
    let super::config::WikiArtifacts {
        nodes,
        edges,
        wiki_graph,
        community_nodes,
        community_edges,
        community_version,
        graph_version,
        repo_map,
        unresolved_report,
        out_dir,
        repo_name,
        bodies,
        file_dev_map,
        feature_of,
    } = art;
    let repo_name_display = repo_name.clone();

    // ── No-op gate (P1.1) ──────────────────────────────────────────────────────
    // Read current HEAD and compute a hash of the flags that affect page content.
    // If all three match the stored wiki_meta.json, the output is already up to date.
    let repo_commit = cih_core::git_head(repo);
    let flags_hash = fnv64(&format!(
        "{}\x00{}\x00{}\x00{}\x00{}",
        wiki_mode, grouping, wiki_language, llm_model, PROMPT_VERSION
    ));
    let existing_meta = load_wiki_meta(&out_dir);
    let wiki_up_to_date = is_wiki_up_to_date(&existing_meta, &repo_commit, &graph_version, &flags_hash);
    if check_only {
        if wiki_up_to_date {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "up_to_date",
                        "head": repo_commit,
                        "graph_version": graph_version,
                    }))?
                );
            } else {
                println!(
                    "wiki is up to date (HEAD={}, graph={})",
                    repo_commit.as_deref().unwrap_or("unknown"),
                    graph_version
                );
            }
            return Ok(());
        } else {
            let reason = wiki_stale_reason(&existing_meta, &repo_commit, &graph_version, &flags_hash);
            if json {
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "stale",
                        "reason": reason,
                    }))?
                );
            } else {
                eprintln!("wiki is stale: {reason}");
            }
            std::process::exit(2);
        }
    }
    if wiki_up_to_date {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({"status": "up_to_date"}))?
            );
        } else {
            println!("wiki is up to date — nothing to do");
        }
        return Ok(());
    }
    // ──────────────────────────────────────────────────────────────────────────

    let (adapter, api_key): (Option<Box<dyn crate::llm::LlmAdapter>>, Option<String>) =
        if effective_run_llm || grouping == WikiGrouping::Llm {
            let a = make_adapter(&llm_provider, &llm_base_url, llm_provider_config.as_deref())?;
            let k = if llm_no_call {
                None
            } else {
                resolve_api_key(llm_api_key_env.as_deref())?
            };
            (Some(a), k)
        } else {
            (None, None)
        };

    let evidence_corpus = EvidenceCorpus::load(&evidence_paths)?;

    let (pool, concurrency) = if effective_run_llm {
        let c = llm_concurrency.clamp(1, 32);
        let p = rayon::ThreadPoolBuilder::new()
            .num_threads(c)
            .build()
            .context("failed to build rayon thread pool")?;
        (Some(p), c)
    } else {
        (None, 0usize)
    };

    let llm_params: Option<LlmRunParams<'_>> = adapter.as_ref().map(|a| LlmRunParams {
        adapter: a.as_ref(),
        api_key: api_key.as_deref(),
        model: llm_model,
        max_tokens: llm_max_tokens,
        timeout_secs: llm_timeout_secs,
        retries: llm_retries,
        dry_run: llm_dry_run,
        language: wiki_language,
        debug_evidence: llm_debug_evidence,
    });

    let mut llm_info: Option<WikiLlmInfo> = None;
    let mut class_enrichment_store: Option<ClassEnrichmentStore> = None;
    #[allow(clippy::type_complexity)] // LLM plumbing signature; alias with wiki rework
    let (llm_summaries, controller_summaries): (
        Option<HashMap<String, CommunityLlmSummary>>,
        Option<HashMap<String, ControllerLlmSummary>>,
    ) = if effective_run_llm {
        let cih_dir = repo.join(".cih");
        let prev_store = if incremental {
            load_class_enrichment(&cih_dir)
        } else {
            ClassEnrichmentStore::default()
        };

        tracing::info!(
            concurrency = concurrency,
            model = llm_model,
            provider = %llm_provider,
            "starting class-traversal LLM enrichment"
        );

        let (ctrl_map, comm_map, updated_store) = enrich_classes_for_chains(
            &wiki_graph,
            &nodes,
            repo,
            prev_store,
            adapter
                .as_ref()
                .expect("LLM adapter set when run_llm is active")
                .as_ref(),
            api_key.as_deref(),
            llm_model,
            llm_max_tokens,
            llm_timeout_secs,
            llm_retries,
            wiki_language,
            llm_dry_run || llm_debug_evidence,
            json,
            &filter_route[..],
            pool.as_ref()
                .expect("thread pool set when run_llm is active"),
        )?;

        tracing::info!(
            classes_in_cache = updated_store.entries.len(),
            comm_summaries = comm_map.len(),
            ctrl_entries = ctrl_map.len(),
            "class-traversal enrichment complete"
        );

        llm_info = Some(WikiLlmInfo {
            provider: llm_provider.to_string(),
            model: llm_model.to_string(),
            language: wiki_language.to_string(),
            evidence_file_count: evidence_corpus.file_count,
            enriched_community_count: comm_map.len(),
            failed_community_count: 0,
            failed_community_ids: vec![],
        });

        class_enrichment_store = Some(updated_store);
        (Some(comm_map), Some(ctrl_map))
    } else {
        (None, None)
    };

    let prev_full_cache: BTreeMap<String, CommunityFullCacheEntry> = if incremental {
        load_wiki_meta(&out_dir)
            .map(|m| m.full_cache)
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let mut full_cache_updates: Vec<(String, String, CommunityLlmFull)> = Vec::new();
    let llm_full_map: Option<HashMap<String, CommunityLlmFull>> =
        if wiki_mode == WikiMode::LlmFull && llm_no_call {
            tracing::info!("skipping llm-full enrichment because dry-run/debug mode is enabled");
            None
        } else if wiki_mode == WikiMode::LlmFull {
            let (map, updates) = run_community_full_enrichment(
                &community_nodes,
                &wiki_graph,
                repo,
                &evidence_corpus,
                pool.as_ref()
                    .expect("thread pool set when run_llm is active"),
                llm_params
                    .as_ref()
                    .expect("LLM params set when run_llm is active"),
                &prev_full_cache,
                json,
            );
            full_cache_updates = updates;
            map
        } else {
            None
        };

    let llm_module_tree: Option<WikiModuleTree> = if grouping == WikiGrouping::Llm && llm_no_call {
        tracing::info!("skipping LLM grouping because dry-run/debug mode is enabled");
        None
    } else if grouping == WikiGrouping::Llm {
        match crate::llm::grouping::propose_module_tree(
            &wiki_graph,
            adapter
                .as_ref()
                .expect("LLM adapter set when run_llm is active")
                .as_ref(),
            api_key.as_deref(),
            llm_model,
            llm_max_tokens,
            llm_timeout_secs,
            &graph_version,
            &community_version,
        ) {
            Ok(tree) => {
                tracing::info!(
                    modules = tree.modules.len(),
                    "LLM grouping proposed {} modules",
                    tree.modules.len()
                );
                Some(tree)
            }
            Err(err) => {
                tracing::warn!(error = %err, "LLM grouping failed; falling back to graph grouping");
                None
            }
        }
    } else {
        None
    };

    let mut feature_cache_updates: Vec<(String, String, FeatureLlmSummary)> = Vec::new();
    let prev_flow_cache: BTreeMap<String, cih_wiki::FlowCacheEntry> = if incremental {
        load_wiki_meta(&out_dir)
            .map(|m| m.flow_cache)
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };
    let feature_llm_map: Option<HashMap<String, FeatureLlmSummary>> = if effective_run_llm {
        let mut feature_groups = group_communities_by_feature(&wiki_graph);
        retain_matching_feature_groups(&mut feature_groups, &filter_feature);
        let prev_meta_for_features: Option<cih_wiki::WikiMeta> = if incremental {
            load_wiki_meta(&out_dir)
        } else {
            None
        };

        let active_features: Vec<&cih_wiki::features::FeatureGroup> = feature_groups
            .iter()
            .filter(|g| !g.community_ids.is_empty())
            .collect();

        let ui_feat = std::sync::Arc::new(std::sync::Mutex::new(crate::ui::PhaseProgress::new()));
        {
            let mut locked = ui_feat.lock().expect("UI progress mutex poisoned");
            if json {
                locked.hide();
            }
            locked.start_phase("Enriching features", Some(active_features.len() as u64));
        }

        use rayon::prelude::*;
        let raw_features: Vec<(String, FeatureLlmSummary, String)> =
            pool.as_ref().expect("thread pool set when run_llm is active").install(|| {
                active_features
                    .par_iter()
                    .filter_map(|group| {
                        let merged_ev = build_feature_evidence(
                            &group.community_ids,
                            &wiki_graph,
                            repo,
                            &evidence_corpus,
                        );
                        let ev_hash = llm_cache_key(&merged_ev, llm_model, wiki_language);
                        let citation_map = build_feature_citation_map(
                            &group.community_ids,
                            &wiki_graph,
                            repo,
                            &evidence_corpus,
                            &file_dev_map,
                        );

                        if let Some(mut cached) = cached_feature_summary(
                            &group.feature,
                            &ev_hash,
                            prev_meta_for_features.as_ref(),
                        ) {
                            resolve_feature_citations(&mut cached, &citation_map);
                            ui_feat
                                .lock()
                                .expect("UI progress mutex poisoned")
                                .tick_skipped(format!("{} (cached)", &group.feature));
                            return Some((group.feature.clone(), cached, ev_hash));
                        }

                        ui_feat.lock().expect("UI progress mutex poisoned").tick(group.feature.as_str());
                        tracing::info!(feature = %group.feature, "calling LLM for feature enrichment");
                        match enrich_one_feature(
                            &group.feature,
                            &merged_ev,
                            adapter.as_ref().expect("LLM adapter set when run_llm is active").as_ref(),
                            api_key.as_deref(),
                            llm_model,
                            llm_max_tokens,
                            llm_timeout_secs,
                            llm_retries,
                            llm_debug_evidence,
                            llm_dry_run,
                        ) {
                            Ok(mut summary) => {
                                resolve_feature_citations(&mut summary, &citation_map);
                                ui_feat.lock().expect("UI progress mutex poisoned").inc_ok();
                                Some((group.feature.clone(), summary, ev_hash))
                            }
                            Err(err) => {
                                tracing::warn!(feature = %group.feature, error = %err, "feature LLM enrichment failed");
                                ui_feat.lock().expect("UI progress mutex poisoned").inc_failed();
                                None
                            }
                        }
                    })
                    .collect()
            });

        ui_feat
            .lock()
            .expect("UI progress mutex poisoned")
            .finish_phase();

        let mut map: HashMap<String, FeatureLlmSummary> = HashMap::new();
        for (feature, summary, ev_hash) in raw_features {
            feature_cache_updates.push((feature.clone(), ev_hash, summary.clone()));
            map.insert(feature, summary);
        }

        tracing::info!(features = map.len(), "feature LLM enrichment complete");
        if map.is_empty() {
            None
        } else {
            Some(map)
        }
    } else {
        None
    };

    let route_flow_scope: Option<std::collections::HashSet<String>> = if !filter_feature.is_empty()
    {
        let mut fg = group_communities_by_feature(&wiki_graph);
        retain_matching_feature_groups(&mut fg, &filter_feature);
        let ids: std::collections::HashSet<String> = fg
            .iter()
            .flat_map(|g| g.community_ids.iter())
            .flat_map(|comm_id| {
                wiki_graph
                    .community_routes
                    .get(comm_id.as_str())
                    .into_iter()
                    .flatten()
                    .map(|(handler, _)| handler.id.as_str().to_string())
            })
            .collect();
        Some(ids)
    } else if !filter_route.is_empty() {
        let ids: std::collections::HashSet<String> = wiki_graph
            .routes
            .iter()
            .filter(|(_, route)| {
                let path = route_path(route);
                filter_route.iter().any(|f| path.contains(f.as_str()))
            })
            .map(|(handler, _)| handler.id.as_str().to_string())
            .collect();
        if ids.is_empty() {
            None
        } else {
            Some(ids)
        }
    } else {
        None
    };

    let flow_llm_map: Option<HashMap<String, FlowLlmSummary>> = if effective_run_llm && !llm_no_call
    {
        let map = run_process_flow_enrichment(
            &wiki_graph,
            llm_params
                .as_ref()
                .expect("LLM params set when run_llm is active"),
            json,
        );
        if map.is_empty() {
            None
        } else {
            Some(map)
        }
    } else {
        None
    };

    let mut flow_cache_updates: Vec<(String, String, FlowLlmSummary)> = Vec::new();
    let flow_llm_map: Option<HashMap<String, FlowLlmSummary>> = if let Some(mut map) = flow_llm_map
    {
        if effective_run_llm && !llm_no_call {
            let (route_flows, updates) = enrich_route_flows(
                &wiki_graph,
                route_flow_scope.as_ref(),
                adapter
                    .as_ref()
                    .expect("LLM adapter set when run_llm is active")
                    .as_ref(),
                api_key.as_deref(),
                llm_model,
                llm_max_tokens,
                llm_timeout_secs,
                llm_retries,
                wiki_language,
                llm_dry_run,
                &prev_flow_cache,
                pool.as_ref()
                    .expect("thread pool set when run_llm is active"),
            );
            flow_cache_updates = updates;
            map.extend(route_flows);
        }
        Some(map)
    } else if effective_run_llm && !llm_no_call {
        let (route_flows, updates) = enrich_route_flows(
            &wiki_graph,
            route_flow_scope.as_ref(),
            adapter
                .as_ref()
                .expect("LLM adapter set when run_llm is active")
                .as_ref(),
            api_key.as_deref(),
            llm_model,
            llm_max_tokens,
            llm_timeout_secs,
            llm_retries,
            wiki_language,
            llm_dry_run,
            &prev_flow_cache,
            pool.as_ref()
                .expect("thread pool set when run_llm is active"),
        );
        flow_cache_updates = updates;
        if route_flows.is_empty() {
            None
        } else {
            Some(route_flows)
        }
    } else {
        flow_llm_map
    };

    let save_evidence_map: Option<HashMap<String, String>> = if save_evidence {
        let map: HashMap<String, String> = community_nodes
            .iter()
            .map(|comm| {
                let pack = crate::llm::evidence::build_evidence_pack(
                    Some(repo),
                    &wiki_graph,
                    comm,
                    &evidence_corpus,
                );
                (comm.id.as_str().to_string(), pack.render())
            })
            .collect();
        Some(map)
    } else {
        None
    };

    let llm_info_for_output = llm_info.clone();

    let entrypoints = {
        let path = repo.join(".cih").join("entrypoints.json");
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                serde_json::from_str::<Vec<cih_wiki::EntrypointRecord>>(&raw).unwrap_or_default()
            }
            Err(_) => Vec::new(),
        }
    };

    let input = WikiInput {
        nodes: &nodes,
        edges: &edges,
        community_nodes: &community_nodes,
        community_edges: &community_edges,
        repo_name,
        graph_version,
        community_version,
        unresolved_report,
        repo_map,
        llm_summaries,
        llm_full: llm_full_map,
        llm_info,
        module_tree: llm_module_tree,
        generation: WikiGenerationInfo {
            mode: wiki_mode.to_string(),
            grouping: grouping.to_string(),
            review_required: false,
            html_viewer: html,
            incremental,
        },
        first_module_tree: None,
        save_evidence: save_evidence_map,
        controller_summaries,
        feature_llm_summaries: feature_llm_map,
        flow_llm_summaries: flow_llm_map,
        grouping: grouping.to_string(),
        filter_feature,
        bodies,
        feature_of,
        entrypoints,
        repo_commit: repo_commit.clone(),
        flags_hash: Some(flags_hash),
    };

    tracing::info!(out_dir = %out_dir.display(), "generating wiki pages");
    let mut ui_gen = crate::ui::PhaseProgress::new();
    if json {
        ui_gen.hide();
    }
    ui_gen.spin("Generating pages");
    let outcome = generate_wiki(input, &out_dir)?;
    ui_gen.finish_with(format!("{} pages", outcome.page_count));

    tracing::info!(
        pages = outcome.page_count,
        communities = outcome.community_count,
        routes = outcome.route_count,
        llm_enriched = outcome.llm_enriched,
        out_dir = %outcome.out_dir.display(),
        "wiki generation complete"
    );

    persist_wiki_meta_caches(&out_dir, &[], &feature_cache_updates, &flow_cache_updates, &full_cache_updates)?;

    if let Some(store) = &class_enrichment_store {
        let cih_dir = repo.join(".cih");
        if let Err(e) = save_class_enrichment(&cih_dir, store) {
            tracing::warn!(error = %e, "failed to save class enrichment cache");
        }
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "out_dir": outcome.out_dir.display().to_string(),
                "manifest_path": outcome.manifest_path.display().to_string(),
                "page_count": outcome.page_count,
                "community_count": outcome.community_count,
                "route_count": outcome.route_count,
                "llm_enriched": outcome.llm_enriched,
                "llm": llm_info_for_output,
            }))?
        );
    } else {
        crate::ui::print_header("Wiki", &repo_name_display, None);
        crate::ui::print_row("Pages", &outcome.page_count.to_string());
        crate::ui::print_row(
            "Communities",
            &format!(
                "{}  routes {}",
                outcome.community_count, outcome.route_count
            ),
        );
        if let Some(ref info) = llm_info_for_output {
            crate::ui::print_row(
                "LLM",
                &format!(
                    "{}  ·  {}  enriched {}  failed {}",
                    info.provider,
                    info.model,
                    info.enriched_community_count,
                    info.failed_community_count
                ),
            );
        }
        crate::ui::print_row("Output", &outcome.out_dir.display().to_string());
        crate::ui::print_row("Manifest", &outcome.manifest_path.display().to_string());
        eprintln!();
    }

    Ok(())
}

/// Returns true when the existing wiki output is current and does not need regeneration.
/// Requires all three signals to match: git HEAD, graph version, and effective wiki flags.
/// For repos without git, HEAD is None and the gate never fires (safe conservative default).
pub(super) fn is_wiki_up_to_date(
    meta: &Option<cih_wiki::WikiMeta>,
    head: &Option<String>,
    graph_version: &str,
    flags_hash: &str,
) -> bool {
    meta.as_ref().is_some_and(|m| {
        m.repo_commit.is_some()
            && m.repo_commit == *head
            && m.graph_version == graph_version
            && m.flags_hash.as_deref() == Some(flags_hash)
    })
}

fn wiki_stale_reason(
    meta: &Option<cih_wiki::WikiMeta>,
    head: &Option<String>,
    graph_version: &str,
    flags_hash: &str,
) -> String {
    let Some(m) = meta else {
        return "wiki has not been generated yet".to_string();
    };
    let mut reasons: Vec<&str> = Vec::new();
    if m.repo_commit.as_deref() != head.as_deref() {
        reasons.push("HEAD changed");
    }
    if m.graph_version != graph_version {
        reasons.push("graph version changed");
    }
    if m.flags_hash.as_deref() != Some(flags_hash) {
        reasons.push("wiki flags changed");
    }
    if reasons.is_empty() {
        "repo_commit is not set in existing wiki_meta.json".to_string()
    } else {
        reasons.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_wiki::WikiMeta;
    use std::collections::BTreeMap;

    fn make_meta(commit: Option<&str>, graph: &str, flags: Option<&str>) -> WikiMeta {
        WikiMeta {
            schema_version: 1,
            repo_commit: commit.map(str::to_string),
            graph_version: graph.to_string(),
            community_version: "v1".to_string(),
            model: None,
            language: None,
            prompt_version: "1".to_string(),
            module_cache: BTreeMap::new(),
            feature_cache: BTreeMap::new(),
            flow_cache: BTreeMap::new(),
            full_cache: BTreeMap::new(),
            flags_hash: flags.map(str::to_string),
        }
    }

    #[test]
    fn gate_fires_when_all_three_signals_match() {
        let meta = Some(make_meta(Some("abc123"), "gv1", Some("fh1")));
        assert!(is_wiki_up_to_date(&meta, &Some("abc123".into()), "gv1", "fh1"));
    }

    #[test]
    fn gate_misses_when_head_changes() {
        let meta = Some(make_meta(Some("abc123"), "gv1", Some("fh1")));
        assert!(!is_wiki_up_to_date(&meta, &Some("def456".into()), "gv1", "fh1"));
    }

    #[test]
    fn gate_misses_when_graph_version_changes() {
        let meta = Some(make_meta(Some("abc123"), "gv1", Some("fh1")));
        assert!(!is_wiki_up_to_date(&meta, &Some("abc123".into()), "gv2", "fh1"));
    }

    #[test]
    fn gate_misses_when_flags_change() {
        let meta = Some(make_meta(Some("abc123"), "gv1", Some("fh1")));
        assert!(!is_wiki_up_to_date(&meta, &Some("abc123".into()), "gv1", "fh2"));
    }

    #[test]
    fn gate_misses_when_no_existing_meta() {
        assert!(!is_wiki_up_to_date(&None, &Some("abc123".into()), "gv1", "fh1"));
    }

    #[test]
    fn gate_misses_for_non_git_repos() {
        // When git_head() returns None, gate never fires even if other signals match.
        let meta = Some(make_meta(Some("abc123"), "gv1", Some("fh1")));
        assert!(!is_wiki_up_to_date(&meta, &None, "gv1", "fh1"));
    }

    #[test]
    fn gate_misses_when_meta_has_no_commit() {
        let meta = Some(make_meta(None, "gv1", Some("fh1")));
        assert!(!is_wiki_up_to_date(&meta, &Some("abc123".into()), "gv1", "fh1"));
    }
}
