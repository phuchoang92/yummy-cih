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
#[derive(Serialize, Deserialize, Default)]
struct RefreshState {
    /// Git HEAD that was current when `analyze` last succeeded.
    #[serde(default)]
    analyze_head: Option<String>,
    /// Graph artifacts version that `discover` was last run against.
    #[serde(default)]
    discover_graph_version: Option<String>,
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

    // ── Staleness warning ─────────────────────────────────────────────────────
    let artifacts_exist = cih_dir.join("artifacts").exists();
    let head_changed = artifacts_exist
        && repo_head.is_some()
        && state.analyze_head.as_deref() != repo_head.as_deref();
    if head_changed && !json {
        eprintln!(
            "warning: graph artifacts are behind HEAD ({}); analyze stage will run",
            repo_head.as_deref().unwrap_or("unknown")
        );
    }

    // ── Analyze stage ─────────────────────────────────────────────────────────
    let head_matches = repo_head.is_some() && state.analyze_head == repo_head;
    let analyze_needed = if args.no_analyze {
        false
    } else {
        args.force || !head_matches || !artifacts_exist
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
                include_decompiled: false,
                scope: None,
                json: false,
                falkor_url: args.db.falkor_url.clone(),
                graph_key: args.db.graph_key.clone(),
                no_load: args.db.no_load,
                no_cache: false,
                skip_xml_integration: false,
                languages: vec![],
                cxf_base_path: None,
            },
        )?;
        let elapsed = t.elapsed();
        // Invalidate discover fingerprint: new graph means new discover needed.
        state.analyze_head = repo_head.clone();
        state.discover_graph_version = None;
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
                &short_sha(state.analyze_head.as_deref())
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
    let discover_needed = if args.no_discover {
        false
    } else {
        args.force || !graph_ver_matches || !community_exists
    };

    let discover_out = if discover_needed {
        let t = Instant::now();
        run_discover(
            repo.clone(),
            args.db.falkor_url.clone(),
            args.db.graph_key.clone(),
            args.db.no_load,
            false,
            DiscoverOverrides {
                community_strategy: "package".to_string(),
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
                &short_sha(current_graph_version.as_deref())
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
    let wiki_grouping: WikiGrouping = args
        .grouping
        .as_deref()
        .unwrap_or("package")
        .parse()
        .context("invalid --grouping")?;
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

    // ── Output ───────────────────────────────────────────────────────────────
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "analyze":  analyze_out,
                "discover": discover_out,
                "wiki":     wiki_out,
            }))?
        );
    } else {
        print_stage("analyze ", &analyze_out);
        print_stage("discover", &discover_out);
        print_stage("wiki    ", &wiki_out);
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
