//! `cih-engine refresh` — analyze → discover → wiki in one shot with per-stage
//! fingerprint skipping. Each stage is skipped when its inputs are unchanged since
//! the last successful run. Staleness warnings surface when the graph is behind HEAD.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::analyze::{run_analyze, AnalyzeFlags};
use crate::discover::{run_discover, DiscoverOverrides, FeatureStrategyKind};
use crate::wiki::{run_wiki, wiki_needs_regen, WikiConfig, WikiGrouping, WikiMode};

use super::args::RefreshArgs;

/// Per-stage fingerprints written to `.cih/refresh-state.json` after each
/// successful stage so subsequent `refresh` calls can skip unchanged stages.
#[derive(Serialize, Deserialize)]
struct RefreshState {
    /// Git HEAD that was current when `analyze` last succeeded.
    #[serde(default)]
    analyze_head: Option<String>,
    /// Git HEAD + parse schema + effective analyze configuration.
    #[serde(default)]
    analyze_fingerprint: Option<String>,
    /// Graph artifacts version that `discover` was last run against.
    #[serde(default)]
    discover_graph_version: Option<String>,
    /// Graph version + community grouping + feature strategy.
    #[serde(default)]
    discover_fingerprint: Option<String>,
    /// True when analyze artifacts exist but have not been confirmed as the
    /// live graph. Legacy state files default to pending because they did not
    /// carry a publication identity.
    #[serde(default = "publication_pending_by_default")]
    analyze_publication_pending: bool,
    /// True when discover overlays exist but have not been published together
    /// with their base analyze artifact.
    #[serde(default = "publication_pending_by_default")]
    discover_publication_pending: bool,
}

const fn publication_pending_by_default() -> bool {
    true
}

impl Default for RefreshState {
    fn default() -> Self {
        Self {
            analyze_head: None,
            analyze_fingerprint: None,
            discover_graph_version: None,
            discover_fingerprint: None,
            analyze_publication_pending: true,
            discover_publication_pending: true,
        }
    }
}

impl RefreshState {
    fn load(cih_dir: &Path) -> Self {
        let path = cih_dir.join("refresh-state.json");
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, cih_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(cih_dir)?;
        let tmp = cih_dir.join("refresh-state.json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, cih_dir.join("refresh-state.json"))?;
        Ok(())
    }
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum StageOutcome {
    Ran { elapsed_ms: u64 },
    Skipped { reason: String },
}

impl StageOutcome {
    fn ran(d: Duration) -> Self {
        Self::Ran {
            elapsed_ms: d.as_millis() as u64,
        }
    }
    fn skipped(reason: impl Into<String>) -> Self {
        Self::Skipped {
            reason: reason.into(),
        }
    }
}

pub fn run(args: RefreshArgs) -> Result<()> {
    let repo = args
        .repo
        .canonicalize()
        .with_context(|| format!("repo path does not exist: {}", args.repo.display()))?;
    let cih_dir = repo.join(".cih");
    let json = args.json;

    let repo_head = cih_core::git_head(&repo);
    let mut state = RefreshState::load(&cih_dir);
    let layers = crate::settings::Layers::load(&repo);
    let analyze_settings =
        crate::settings::resolve_analyze(crate::settings::AnalyzeFlagInputs::default(), &layers);
    let agent_context_settings =
        crate::settings::resolve_agent_context(args.no_agent_context, &layers);
    let current_analyze_fingerprint = analyze_fingerprint(
        repo_head.as_deref(),
        &analyze_settings,
        cih_lang::PARSE_CACHE_SCHEMA,
    );
    let wiki_grouping: WikiGrouping = args
        .grouping
        .as_deref()
        .unwrap_or("package")
        .parse()
        .context("invalid --grouping")?;
    let community_strategy = match wiki_grouping {
        WikiGrouping::Graph => "graph",
        WikiGrouping::Package | WikiGrouping::Llm => "package",
    };

    // ── Staleness warning ─────────────────────────────────────────────────────
    let artifacts_exist = cih_dir.join("artifacts").exists();
    let analyze_inputs_changed = artifacts_exist
        && state.analyze_fingerprint.as_deref() != Some(&current_analyze_fingerprint);
    if analyze_inputs_changed && !json {
        eprintln!(
            "warning: analyze inputs changed (HEAD {}); analyze stage will run",
            repo_head.as_deref().unwrap_or("unknown")
        );
    }

    // ── Analyze stage ─────────────────────────────────────────────────────────
    // Without a Git HEAD there is no stable source-content component to
    // compare. Preserve the previous safe behaviour for unpacked/non-git
    // repositories and re-run analyze instead of treating an empty HEAD as a
    // durable cache hit.
    let analyze_fingerprint_matches = analyze_fingerprint_is_current(
        repo_head.as_deref(),
        state.analyze_fingerprint.as_deref(),
        &current_analyze_fingerprint,
    );
    let analyze_needed = if args.no_analyze {
        false
    } else {
        args.force
            || state.analyze_publication_pending
            || !analyze_fingerprint_matches
            || !artifacts_exist
    };

    let analyze_out = if analyze_needed {
        let t = Instant::now();
        run_analyze(
            repo.clone(),
            AnalyzeFlags {
                all: true,
                modules: vec![],
                include: vec![],
                exclude: vec![],
                include_decompiled: analyze_settings.include_decompiled,
                scope: None,
                json: false,
                backend: args.db.backend.clone(),
                falkor_url: args.db.falkor_url.clone(),
                graph_key: args.db.graph_key.clone(),
                no_load: args.db.no_load,
                no_cache: false,
                skip_xml_integration: analyze_settings.skip_xml_integration,
                languages: analyze_settings.languages.clone(),
                route_base_path: analyze_settings.cxf_base_path.clone(),
                sql_apis: analyze_settings.sql_apis.clone(),
            },
        )?;
        let elapsed = t.elapsed();
        // Invalidate discover fingerprint: new graph means new discover needed.
        state.analyze_head = repo_head.clone();
        state.analyze_fingerprint = Some(current_analyze_fingerprint.clone());
        state.analyze_publication_pending = args.db.no_load;
        state.discover_graph_version = None;
        state.discover_fingerprint = None;
        state.discover_publication_pending = true;
        if let Err(e) = state.save(&cih_dir) {
            tracing::warn!(error = %e, "failed to save refresh state after analyze");
        }
        StageOutcome::ran(elapsed)
    } else {
        let reason = if args.no_analyze {
            "--no-analyze".to_string()
        } else {
            format!(
                "up to date (HEAD {})",
                short_sha(state.analyze_head.as_deref())
            )
        };
        StageOutcome::skipped(reason)
    };

    // ── Discover stage ────────────────────────────────────────────────────────
    let current_graph_version = crate::versioning::latest_graph_artifacts(&repo)
        .map(|a| a.version.to_string())
        .ok();
    let community_exists = cih_dir.join("artifacts-community").exists();
    let graph_ver_matches = current_graph_version
        .as_deref()
        .is_some_and(|v| state.discover_graph_version.as_deref() == Some(v));
    let current_discover_fingerprint = discover_fingerprint(
        current_graph_version.as_deref(),
        community_strategy,
        FeatureStrategyKind::Package,
    );
    let discover_fingerprint_matches =
        state.discover_fingerprint.as_deref() == Some(&current_discover_fingerprint);
    let discover_needed = if args.no_discover {
        false
    } else {
        args.force
            || state.discover_publication_pending
            || !graph_ver_matches
            || !discover_fingerprint_matches
            || !community_exists
    };

    let discover_out = if discover_needed {
        let t = Instant::now();
        run_discover(
            repo.clone(),
            args.db.backend.clone(),
            args.db.falkor_url.clone(),
            args.db.graph_key.clone(),
            args.db.no_load,
            false,
            DiscoverOverrides {
                community_strategy: community_strategy.to_string(),
                resolution: None,
                min_community_size: None,
                max_trace_depth: None,
                max_processes: None,
                max_branching: None,
                min_trace_confidence: None,
                feature_strategy: FeatureStrategyKind::Package,
                feature_llm: None,
                pg_url: None,
                embed_similarity_threshold: None,
                embed_knn: None,
                embed_leiden_resolution: None,
            },
        )?;
        let elapsed = t.elapsed();
        state.discover_graph_version = current_graph_version.clone();
        state.discover_fingerprint = Some(current_discover_fingerprint);
        state.discover_publication_pending = args.db.no_load;
        if let Err(e) = state.save(&cih_dir) {
            tracing::warn!(error = %e, "failed to save refresh state after discover");
        }
        StageOutcome::ran(elapsed)
    } else {
        let reason = if args.no_discover {
            "--no-discover".to_string()
        } else {
            format!(
                "up to date (graph {})",
                short_sha(current_graph_version.as_deref())
            )
        };
        StageOutcome::skipped(reason)
    };

    // ── Wiki stage ───────────────────────────────────────────────────────────
    let wiki_mode: WikiMode = args
        .wiki_mode
        .as_deref()
        .unwrap_or("graph")
        .parse()
        .context("invalid --wiki-mode")?;
    let wiki_language = args.wiki_language.as_deref().unwrap_or("en").to_string();
    let llm_model = args.llm_model.as_deref().unwrap_or("").to_string();
    let out_dir = args
        .wiki_out
        .clone()
        .unwrap_or_else(|| cih_dir.join("wiki"));

    let wiki_stale = if args.no_wiki {
        false
    } else if args.force {
        true
    } else {
        wiki_needs_regen(
            &repo,
            &out_dir,
            wiki_mode,
            wiki_grouping,
            &wiki_language,
            &llm_model,
        )
    };

    let wiki_out = if !args.no_wiki && wiki_stale {
        let t = Instant::now();
        run_wiki(WikiConfig {
            repo: repo.clone(),
            out: args.wiki_out.clone(),
            run_llm: args.llm,
            llm: crate::llm::LlmCallConfig {
                provider: args
                    .llm_provider
                    .as_deref()
                    .unwrap_or("openai-compatible")
                    .parse()
                    .unwrap_or_default(),
                api_key_env: args.llm_api_key_env.clone(),
                model: llm_model,
                ..Default::default()
            },
            wiki_mode,
            grouping: wiki_grouping,
            wiki_language,
            stage_and_swap: args.stage_and_swap,
            json: false,
            ..WikiConfig::default()
        })?;
        StageOutcome::ran(t.elapsed())
    } else {
        let reason = if args.no_wiki {
            "--no-wiki".to_string()
        } else {
            "up to date".to_string()
        };
        StageOutcome::skipped(reason)
    };

    // Agent context is refreshed even when the graph/wiki stages are cache hits,
    // so new templates and managed-instruction migrations still take effect.
    let agent_context_out = if agent_context_settings.enabled {
        let t = Instant::now();
        crate::agent_context::generate_repo_agent_context(
            &repo,
            crate::agent_context::AgentContextOptions {
                enabled: true,
                area_skills: agent_context_settings.area_skills,
            },
        )?;
        StageOutcome::ran(t.elapsed())
    } else {
        StageOutcome::skipped(if args.no_agent_context {
            "--no-agent-context"
        } else {
            "disabled by [agent_context].enabled"
        })
    };

    // ── Output ───────────────────────────────────────────────────────────────
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "analyze":  analyze_out,
                "discover": discover_out,
                "wiki":     wiki_out,
                "agent_context": agent_context_out,
            }))?
        );
    } else {
        print_stage("analyze ", &analyze_out);
        print_stage("discover", &discover_out);
        print_stage("wiki    ", &wiki_out);
        print_stage("agents  ", &agent_context_out);
    }

    Ok(())
}

fn print_stage(name: &str, out: &StageOutcome) {
    match out {
        StageOutcome::Ran { elapsed_ms } => {
            eprintln!("  {name}  ran     ({elapsed_ms} ms)");
        }
        StageOutcome::Skipped { reason } => {
            eprintln!("  {name}  skipped ({reason})");
        }
    }
}

fn short_sha(s: Option<&str>) -> String {
    s.map(|h| h[..h.len().min(8)].to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn analyze_fingerprint_is_current(
    git_head: Option<&str>,
    stored_fingerprint: Option<&str>,
    current_fingerprint: &str,
) -> bool {
    git_head.is_some() && stored_fingerprint == Some(current_fingerprint)
}

fn analyze_fingerprint(
    git_head: Option<&str>,
    settings: &crate::settings::AnalyzeResolved,
    parse_schema: u32,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cih-refresh-analyze-v1\0");
    hash_part(&mut hasher, git_head.unwrap_or(""));
    hash_part(&mut hasher, &parse_schema.to_string());
    hash_part(&mut hasher, &settings.languages.join("\0"));
    hash_part(
        &mut hasher,
        if settings.skip_xml_integration {
            "1"
        } else {
            "0"
        },
    );
    hash_part(
        &mut hasher,
        if settings.include_decompiled {
            "1"
        } else {
            "0"
        },
    );
    hash_part(&mut hasher, settings.cxf_base_path.as_deref().unwrap_or(""));
    hash_part(&mut hasher, &settings.sql_apis.join("\0"));
    hasher.finalize().to_hex().to_string()
}

fn discover_fingerprint(
    graph_version: Option<&str>,
    grouping: &str,
    feature_strategy: FeatureStrategyKind,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"cih-refresh-discover-v1\0");
    hash_part(&mut hasher, graph_version.unwrap_or(""));
    hash_part(&mut hasher, grouping);
    hash_part(&mut hasher, &feature_strategy.to_string());
    hasher.finalize().to_hex().to_string()
}

fn hash_part(hasher: &mut blake3::Hasher, value: &str) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_refresh_state_requires_publication_verification() {
        let state: RefreshState =
            serde_json::from_str(r#"{"analyze_head":"abc","discover_graph_version":"graph-v1"}"#)
                .expect("legacy refresh state should deserialize");

        assert!(state.analyze_publication_pending);
        assert!(state.discover_publication_pending);
        assert!(state.analyze_fingerprint.is_none());
        assert!(state.discover_fingerprint.is_none());
    }

    #[test]
    fn verified_refresh_state_round_trips() {
        let state = RefreshState {
            analyze_head: Some("abc".to_string()),
            analyze_fingerprint: Some("analyze-v1".to_string()),
            discover_graph_version: Some("graph-v1".to_string()),
            discover_fingerprint: Some("discover-v1".to_string()),
            analyze_publication_pending: false,
            discover_publication_pending: false,
        };

        let encoded = serde_json::to_string(&state).expect("serialize refresh state");
        let decoded: RefreshState =
            serde_json::from_str(&encoded).expect("deserialize refresh state");

        assert!(!decoded.analyze_publication_pending);
        assert!(!decoded.discover_publication_pending);
        assert_eq!(decoded.analyze_fingerprint.as_deref(), Some("analyze-v1"));
        assert_eq!(decoded.discover_fingerprint.as_deref(), Some("discover-v1"));
    }

    #[test]
    fn refresh_fingerprints_track_schema_grouping_and_feature_strategy() {
        let settings = crate::settings::resolve_analyze(
            crate::settings::AnalyzeFlagInputs::default(),
            &crate::settings::Layers::default(),
        );
        assert_ne!(
            analyze_fingerprint(Some("abc"), &settings, 27),
            analyze_fingerprint(Some("abc"), &settings, 28)
        );
        assert_ne!(
            discover_fingerprint(Some("graph-v1"), "package", FeatureStrategyKind::Package),
            discover_fingerprint(Some("graph-v1"), "graph", FeatureStrategyKind::Package)
        );
        assert_ne!(
            discover_fingerprint(Some("graph-v1"), "graph", FeatureStrategyKind::Package),
            discover_fingerprint(Some("graph-v1"), "graph", FeatureStrategyKind::Structural)
        );
    }

    #[test]
    fn non_git_repo_never_treats_an_empty_head_as_a_cache_hit() {
        assert!(analyze_fingerprint_is_current(
            Some("abc"),
            Some("fingerprint"),
            "fingerprint"
        ));
        assert!(!analyze_fingerprint_is_current(
            None,
            Some("fingerprint"),
            "fingerprint"
        ));
    }
}
