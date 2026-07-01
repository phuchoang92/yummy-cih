mod analyze;
mod cmd;
mod db;
mod decompile;
mod decompile_config;
mod discover;
mod taint_config;
mod embed;
mod feature_strategy;
mod file_cache;
mod group_sync;
mod llm;
mod node_prefix;
mod registry;
mod runtime;
mod scan;
mod scope;
mod start;
mod start_env;

mod tui;
mod ui;
mod versioning;
mod wiki;

use std::path::PathBuf;

use analyze::AnalyzeFlags;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

/// Default FalkorDB URL (Homebrew redis squats 6379, FalkorDB on 6380).
const DEFAULT_FALKOR_URL: &str = "redis://127.0.0.1:6380";
const DEFAULT_GRAPH_KEY: &str = "cih";

/// Shared FalkorDB connection + load options, used by Analyze, Resolve, and Discover.
#[derive(Debug, clap::Args)]
struct DbArgs {
    /// FalkorDB URL. Defaults to $FALKOR_URL or redis://127.0.0.1:6380.
    #[arg(long, env = "FALKOR_URL")]
    falkor_url: Option<String>,
    /// FalkorDB graph key. Defaults to $CIH_GRAPH_KEY or "cih".
    #[arg(long, env = "CIH_GRAPH_KEY")]
    graph_key: Option<String>,
    /// Skip the FalkorDB load step (emit JSONL artifacts only).
    #[arg(long)]
    no_load: bool,
}

#[derive(Debug, Parser)]
#[command(name = "cih-engine", about = "Code Intelligence Hub engine CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Fast repository discovery pass. Writes .cih/repo-map.json.
    Scan {
        /// Repository root to scan.
        repo: PathBuf,
        /// Print RepoMap JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// Parse selected files, emit structure graph, and load into FalkorDB.
    Analyze {
        /// Repository root to analyze.
        repo: Option<PathBuf>,
        /// Select all Java files, excluding decompiled dirs unless requested.
        #[arg(long)]
        all: bool,
        /// Select one or more module names, comma-delimited or repeated.
        #[arg(long = "module", value_delimiter = ',')]
        modules: Vec<String>,
        /// Include Java files matching this repo-relative glob. Can be repeated.
        #[arg(long)]
        include: Vec<String>,
        /// Exclude Java files matching this repo-relative glob. Can be repeated.
        #[arg(long)]
        exclude: Vec<String>,
        /// Include files under decompiled dirs such as .workspace-dependencies.
        #[arg(long)]
        include_decompiled: bool,
        /// Scope TOML file. Defaults to `<repo>/cih.scope.toml` when present.
        #[arg(long)]
        scope: Option<PathBuf>,
        /// Print the resolved ScopeFile JSON instead of the human summary.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        db: DbArgs,
        /// Disable incremental parse cache and re-parse all files.
        #[arg(long)]
        no_cache: bool,
        /// Skip integration and DI XML extraction (faster on large repos).
        #[arg(long)]
        skip_xml_integration: bool,
        /// Limit analysis to these languages (comma-delimited or repeated). Default: all.
        /// Example: --language java,typescript
        #[arg(long = "language", value_delimiter = ',')]
        languages: Vec<String>,
    },
    /// Re-run the resolve pass using the saved scope (.cih/scope.json), without re-scanning.
    /// Useful when the resolver changes but the source files have not.
    Resolve {
        /// Repository root (must contain .cih/scope.json from a prior `analyze` run).
        repo: PathBuf,
        #[command(flatten)]
        db: DbArgs,
        /// Print the summary as JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// Detect communities and process traces from the latest analyzed artifacts.
    Discover {
        /// Repository root with `.cih/artifacts/<version>` from a prior analyze/resolve run.
        repo: PathBuf,
        #[command(flatten)]
        db: DbArgs,
        /// Print the summary as JSON instead of the human summary.
        #[arg(long)]
        json: bool,

        // ── Community detection overrides ──────────────────────────────────
        /// Community detection strategy.
        /// "package" (default): groups by package/module structure — one community per feature.
        /// "graph": Leiden graph-clustering — groups by call-graph connectivity.
        #[arg(long, default_value = "package")]
        community_strategy: String,
        /// Leiden resolution (only used with --community-strategy graph).
        /// Higher = more, smaller communities; lower = fewer, larger ones. Default: 1.0.
        #[arg(long)]
        resolution: Option<f64>,
        /// Minimum community size (only used with --community-strategy graph).
        /// Drop communities smaller than this many members. Default: 2 (3 for large graphs).
        #[arg(long)]
        min_community_size: Option<usize>,

        // ── Process trace overrides ────────────────────────────────────────
        /// Maximum BFS depth per process trace. Default: 10.
        #[arg(long)]
        max_trace_depth: Option<usize>,
        /// Maximum number of processes kept after deduplication. Default: scales with codebase size.
        #[arg(long)]
        max_processes: Option<usize>,
        /// Maximum call-graph branching factor per BFS step. Default: 4.
        #[arg(long)]
        max_branching: Option<usize>,
        /// Minimum edge confidence to follow during BFS (0.0–1.0). Default: 0.5.
        #[arg(long)]
        min_trace_confidence: Option<f32>,

        // ── Feature grouping strategy ──────────────────────────────────────
        /// Feature classification strategy: package (default), structural, hybrid, llm.
        /// "hybrid" runs structural → package → embed → llm (if --feature-llm-provider set).
        /// "llm" requires --feature-llm-provider.
        #[arg(long, default_value = "package")]
        feature_strategy: String,
        /// LLM provider for feature classification.
        /// One of: deepseek, gemini, anthropic, openai-compatible.
        /// Required when --feature-strategy is llm or hybrid with LLM stage.
        #[arg(long)]
        feature_llm_provider: Option<String>,
        /// LLM model for feature classification.
        /// Defaults: deepseek-chat (deepseek), gemini-2.5-flash (gemini),
        /// claude-haiku-4-5-20251001 (anthropic), gpt-4o-mini (openai-compatible).
        #[arg(long, default_value = "")]
        feature_llm_model: String,
        /// Base URL for --feature-llm-provider openai-compatible.
        #[arg(long, default_value = "https://api.openai.com/v1")]
        feature_llm_base_url: String,
        /// Override which env var holds the API key for feature LLM.
        /// Defaults to auto-detect (CIH_LLM_API_KEY, DEEPSEEK_API_KEY, etc.).
        #[arg(long)]
        feature_llm_api_key_env: Option<String>,
        /// Maximum output tokens per feature LLM call.
        #[arg(long, default_value = "2048")]
        feature_llm_max_tokens: u32,
        /// Timeout in seconds per feature LLM API call.
        #[arg(long, default_value = "60")]
        feature_llm_timeout_secs: u64,
    },
    /// Embed searchable graph nodes from the latest analyzed artifacts into pgvector.
    Embed {
        /// Repository root with `.cih/artifacts/<version>` from a prior analyze/resolve run.
        repo: PathBuf,
        /// Postgres connection URL. Defaults to $CIH_PG_URL.
        #[arg(long, env = "CIH_PG_URL")]
        pg_url: Option<String>,
        /// Embedding model: all-minilm-l6-v2 or bge-small-en-v1.5.
        #[arg(long, default_value = "all-minilm-l6-v2")]
        model: String,
        /// Print the summary as JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// List all repos registered in ~/.cih/registry.json.
    List {
        /// Print registry entries as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the registry status for one repo (name or absolute path).
    Status {
        /// Repo name or absolute path as registered.
        name: String,
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Manage cross-service repo groups and sync contract matches.
    Group {
        #[command(subcommand)]
        command: GroupCommand,
    },
    /// Generate a role-based wiki bundle from existing graph artifacts.
    Wiki {
        /// Repository root with `.cih/artifacts/` and `.cih/artifacts-community/` from prior runs.
        repo: PathBuf,
        /// Output directory. Defaults to `<repo>/.cih/wiki`.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Enable LLM enrichment. Set an API key env var before using:
        /// DEEPSEEK_API_KEY, GEMINI_API_KEY, OPENAI_API_KEY, ANTHROPIC_API_KEY, or CIH_LLM_API_KEY.
        #[arg(long, env = "CIH_LLM")]
        llm: bool,
        /// Deprecated: alias for --llm. Will be removed in a future release.
        #[arg(long, env = "CIH_LLM_ENRICH", hide = true)]
        llm_enrich: bool,
        /// LLM provider adapter. One of:
        ///   deepseek          — DeepSeek API (DEEPSEEK_API_KEY, model: deepseek-chat)
        ///   gemini            — Google Gemini API (GEMINI_API_KEY, model: gemini-2.5-flash)
        ///   anthropic         — Anthropic API (ANTHROPIC_API_KEY, model: claude-haiku-4-5-20251001)
        ///   openai-compatible — Any OpenAI-compatible endpoint (use with --llm-base-url)
        ///   http-json         — Custom HTTP adapter (use with --llm-provider-config)
        #[arg(long, default_value = "openai-compatible")]
        llm_provider: String,
        /// JSON config file for --llm-provider http-json.
        #[arg(long)]
        llm_provider_config: Option<PathBuf>,
        /// Override which env var holds the API key. Defaults to auto-detect from provider.
        #[arg(long)]
        llm_api_key_env: Option<String>,
        /// External evidence file (.md or .txt) to include in LLM wiki prompts.
        #[arg(long = "evidence")]
        evidence: Vec<PathBuf>,
        /// Base URL for --llm-provider openai-compatible. Ignored for deepseek/gemini/anthropic.
        #[arg(long, default_value = "https://api.openai.com/v1")]
        llm_base_url: String,
        /// Model name for LLM enrichment. Provider-specific defaults:
        ///   deepseek-chat (deepseek), gemini-2.5-flash (gemini),
        ///   claude-haiku-4-5-20251001 (anthropic), gpt-4o-mini (openai-compatible).
        #[arg(long, default_value = "")]
        llm_model: String,
        /// Maximum output tokens per LLM call. Increase to 4096 for Gemini to avoid truncation.
        #[arg(long, default_value = "600")]
        llm_max_tokens: u32,
        /// Timeout in seconds per LLM API call.
        #[arg(long, default_value = "30")]
        llm_timeout_secs: u64,
        /// Retries on transient LLM failures.
        #[arg(long, default_value = "2")]
        llm_retries: u32,
        /// Maximum concurrent LLM calls.
        #[arg(long, default_value = "8")]
        llm_concurrency: usize,
        /// Print evidence packs to stdout instead of calling the LLM.
        #[arg(long)]
        llm_debug_evidence: bool,
        /// Print prompts to stdout without calling the LLM (dry run).
        #[arg(long)]
        llm_dry_run: bool,
        /// Documentation language for LLM-generated text.
        #[arg(long, default_value = "en")]
        wiki_language: String,
        /// Wiki generation mode: graph (no LLM), llm-summary, or llm-full.
        #[arg(long, default_value = "graph")]
        wiki_mode: String,
        /// Community grouping strategy: package (by Java package path, default), graph (Leiden communities), or llm (LLM-proposed).
        #[arg(long, default_value = "package")]
        grouping: String,
        /// Write a standalone index.html viewer alongside the Markdown files.
        #[arg(long)]
        html: bool,
        /// Skip communities whose evidence hash has not changed since the last run.
        #[arg(long)]
        incremental: bool,
        /// Save per-community evidence packs to .cih/wiki/evidence/<slug>.json.
        #[arg(long = "save-evidence")]
        save_evidence: bool,
        /// Only generate docs for communities whose name contains this substring (case-insensitive).
        /// Can be specified multiple times to include several groups.
        #[arg(long = "filter-community")]
        filter_community: Vec<String>,
        /// Only generate pages for features (module directories) whose name contains this substring
        /// (case-insensitive). Works with or without a prior `discover` run.
        /// Can be specified multiple times.
        #[arg(long = "filter-feature")]
        filter_feature: Vec<String>,
        /// Only generate pages for communities that own at least one route whose path starts
        /// with or contains this pattern (case-insensitive).
        /// Can be specified multiple times to match several prefixes.
        /// Example: --filter-route /api/payment --filter-route /api/order
        #[arg(long = "filter-route")]
        filter_route: Vec<String>,
        /// Limit total number of communities processed (useful for quick tests).
        #[arg(long)]
        max_communities: Option<usize>,
        /// Print outcome as JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// Inspect and manage feature grouping assignments.
    Features {
        #[command(subcommand)]
        command: FeaturesCommand,
    },
    /// Run Phase 0 + Phase 1 + Phase 2 taint analysis on the latest graph artifacts.
    /// Phase 0: BFS on method-granularity call graph (inter-procedural).
    /// Phase 1: intra-procedural IR for source methods (confirms/penalises paths).
    /// Phase 2: on-demand CFG construction + dominance tree for confirmed source methods.
    /// Requires a prior `analyze` run. Emits TaintFlow edges to .cih/artifacts-taint/.
    Taint {
        /// Repository root with `.cih/artifacts/` from a prior analyze run.
        repo: PathBuf,
        #[command(flatten)]
        db: DbArgs,
        /// Disable intra-procedural liveness analysis (faster, more false positives).
        #[arg(long = "no-intra-proc", default_value_t = true, action = clap::ArgAction::SetFalse)]
        intra_proc: bool,
        /// Disable CFG construction and dominance-tree analysis.
        #[arg(long = "no-cfg", default_value_t = true, action = clap::ArgAction::SetFalse)]
        cfg: bool,
        /// Disable PDG-based flow-sensitive taint analysis.
        #[arg(long = "no-pdg", default_value_t = true, action = clap::ArgAction::SetFalse)]
        pdg: bool,
        /// Print results as JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// Edit CIH configuration files interactively.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Open the interactive TUI command builder.
    /// Navigate commands on the left, fill options on the right,
    /// then press r to review and run the assembled command.
    Ui,
    /// Interactive guided setup wizard for CIH.
    Start {
        /// CIH workspace directory containing docker-compose.yml. Default: current directory.
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// Target Java/Spring repository path. Required when --non-interactive.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Repository name for docs viewer URL prefix. Default: derived from repo path.
        #[arg(long)]
        repo_name: Option<String>,
        /// Postgres password written to .env. Required in --non-interactive mode
        /// (or read from the POSTGRES_PASSWORD env var).
        #[arg(long)]
        postgres_password: Option<String>,
        /// Print plan without writing files or running commands.
        #[arg(long)]
        dry_run: bool,
        /// Skip interactive prompts (requires --repo).
        #[arg(long)]
        non_interactive: bool,
    },
    /// Export, import, or bootstrap CIH bundle archives (Gap 5).
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ArtifactCommand {
    /// Export the current .cih/ state to a bundle archive.
    Export {
        /// Repository root (must contain .cih/ from a prior analyze run).
        repo: PathBuf,
        /// Output bundle path (default: <repo>/.cih/graph.db.zst).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Import a bundle archive into .cih/ (restores incremental state).
    Import {
        /// Repository root to restore into.
        repo: PathBuf,
        /// Bundle archive path.
        #[arg(long)]
        bundle: PathBuf,
    },
    /// Import a bundle and bulk-load into FalkorDB, then register in registry.
    Bootstrap {
        /// Repository root.
        repo: PathBuf,
        /// Bundle archive path.
        #[arg(long)]
        bundle: PathBuf,
        /// FalkorDB URL.
        #[arg(long, env = "FALKOR_URL")]
        falkor_url: Option<String>,
        /// FalkorDB graph key.
        #[arg(long, env = "CIH_GRAPH_KEY")]
        graph_key: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Interactively edit decompile settings (JAR directories, prefixes, tool path).
    Decompile {
        /// Repository root. Reads and writes `<repo>/cih.decompile.toml`.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum GroupCommand {
    /// Create an empty repo group.
    Create {
        /// Group name.
        name: String,
    },
    /// Add a registered repo to a group.
    Add {
        /// Group name.
        name: String,
        /// Registered repo name or absolute path.
        repo: String,
    },
    /// Remove a repo from a group.
    Remove {
        /// Group name.
        name: String,
        /// Registered repo name or absolute path.
        repo: String,
    },
    /// List repo groups.
    List {
        /// Print groups as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Sync cross-service contract matches for a group.
    Sync {
        /// Group name.
        name: String,
        /// FalkorDB URL accepted for forward compatibility; sync reads local artifacts today.
        #[arg(long, env = "FALKOR_URL")]
        falkor_url: Option<String>,
        /// Print sync summary as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum FeaturesCommand {
    /// Print the current feature groupings table (reads .cih/artifacts-features/).
    /// Run `discover` first to generate the artifact.
    Show {
        /// Repository root with `.cih/artifacts-features/` from a prior discover run.
        repo: PathBuf,
        /// Print as JSON instead of the human table.
        #[arg(long)]
        json: bool,
    },
    /// Add or update a manual override in .cih/feature-overrides.json.
    /// Re-run `discover` to apply the override to the artifact.
    Override {
        /// Repository root.
        repo: PathBuf,
        /// Node ID to lock (e.g. "Class:com.example.PaymentService").
        node_id: String,
        /// Feature slug to assign (e.g. "payment").
        feature: String,
        /// Optional human-readable reason stored in the sidecar.
        #[arg(long, default_value = "")]
        reason: String,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    runtime::init()?;

    let cli = Cli::parse();

    // TUI command builder — runs before the normal dispatch so the terminal is
    // restored before we print anything or exec the chosen command.
    if matches!(cli.command, Command::Ui) {
        if let Some(args) = tui::run_tui()? {
            let cmd_display = std::iter::once("cih-engine")
                .chain(args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            println!();
            println!("  Running: {}", cmd_display);
            println!();
            let exe =
                std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("cih-engine"));
            let status = std::process::Command::new(&exe).args(&args).status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        return Ok(());
    }

    match cli.command {
        Command::Scan { repo, json } => scan::run_scan(&repo, json),
        Command::Analyze {
            repo,
            all,
            modules,
            include,
            exclude,
            include_decompiled,
            scope,
            json,
            db,
            no_cache,
            skip_xml_integration,
            languages,
        } => {
            let repo = match repo {
                Some(r) => r,
                None => std::env::current_dir().with_context(|| {
                    "failed to determine current working directory — pass an explicit repo path or run from a valid directory"
                })?,
            };
            analyze::run_analyze(
                repo,
                AnalyzeFlags {
                    all,
                    modules,
                    include,
                    exclude,
                    include_decompiled,
                    scope,
                    json,
                    falkor_url: db.falkor_url,
                    graph_key: db.graph_key,
                    no_load: db.no_load,
                    no_cache,
                    skip_xml_integration,
                    languages,
                },
            )
        }
        Command::Resolve {
            repo,
            db,
            json,
        } => analyze::run_resolve(repo, db.falkor_url, db.graph_key, db.no_load, json),
        Command::Discover {
            repo,
            db,
            json,
            community_strategy,
            resolution,
            min_community_size,
            max_trace_depth,
            max_processes,
            max_branching,
            min_trace_confidence,
            feature_strategy,
            feature_llm_provider,
            feature_llm_model,
            feature_llm_base_url,
            feature_llm_api_key_env,
            feature_llm_max_tokens,
            feature_llm_timeout_secs,
        } => {
            // Build optional LLM config when a provider is specified.
            let feature_llm = feature_llm_provider
                .map(|s| s.parse::<llm::LlmProvider>())
                .transpose()?
                .map(|provider| {
                    let model = if feature_llm_model.is_empty() {
                        match provider {
                            llm::LlmProvider::DeepSeek => "deepseek-chat".to_string(),
                            llm::LlmProvider::Gemini => "gemini-2.5-flash".to_string(),
                            llm::LlmProvider::Anthropic => "claude-haiku-4-5-20251001".to_string(),
                            _ => "gpt-4o-mini".to_string(),
                        }
                    } else {
                        feature_llm_model.clone()
                    };
                    llm::LlmCallConfig {
                        provider,
                        base_url: feature_llm_base_url,
                        model,
                        api_key_env: feature_llm_api_key_env,
                        max_tokens: feature_llm_max_tokens,
                        timeout_secs: feature_llm_timeout_secs,
                        retries: 0,
                    }
                });
            discover::run_discover(
                repo,
                db.falkor_url,
                db.graph_key,
                db.no_load,
                json,
                discover::DiscoverOverrides {
                    community_strategy,
                    resolution,
                    min_community_size,
                    max_trace_depth,
                    max_processes,
                    max_branching,
                    min_trace_confidence,
                    feature_strategy: feature_strategy.parse().unwrap_or_default(),
                    feature_llm,
                },
            )
        }
        Command::Embed {
            repo,
            pg_url,
            model,
            json,
        } => embed::run_embed(repo, pg_url, model, json),
        Command::List { json } => {
            use cih_core::Registry;
            let reg = Registry::load();
            if json {
                println!("{}", serde_json::to_string_pretty(&reg)?);
            } else {
                if reg.entries.is_empty() {
                    println!("No repositories indexed yet. Run `cih-engine analyze <repo>` first.");
                } else {
                    println!(
                        "{:<24} {:<12} {:>8} {:>8} {:>6}  path",
                        "name", "indexed_at", "nodes", "edges", "files"
                    );
                    println!("{}", "-".repeat(90));
                    for e in &reg.entries {
                        let date = e.indexed_at.get(..10).unwrap_or(&e.indexed_at);
                        println!(
                            "{:<24} {:<12} {:>8} {:>8} {:>6}  {}",
                            e.name, date, e.stats.nodes, e.stats.edges, e.stats.files, e.path
                        );
                    }
                }
            }
            Ok(())
        }
        Command::Status { name, json } => {
            use cih_core::Registry;
            let reg = Registry::load();
            if let Some(entry) = reg.find(&name) {
                let stale = reg.is_stale(&name);
                let repo_path = std::path::Path::new(&entry.path);
                let feat_status = cmd::features::load_feature_status(repo_path);
                if json {
                    #[derive(serde::Serialize)]
                    struct FeatureInfo {
                        feature_count: usize,
                        node_count: usize,
                        pinned_count: usize,
                        strategy: String,
                        graph_version: String,
                    }
                    #[derive(serde::Serialize)]
                    struct StatusOutput<'a> {
                        entry: &'a cih_core::RegistryEntry,
                        stale: bool,
                        #[serde(skip_serializing_if = "Option::is_none")]
                        features: Option<FeatureInfo>,
                    }
                    let features = feat_status.map(|fs| FeatureInfo {
                        feature_count: fs.feature_count,
                        node_count: fs.node_count,
                        pinned_count: fs.pinned_count,
                        strategy: fs.strategy,
                        graph_version: fs.graph_version,
                    });
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&StatusOutput {
                            entry,
                            stale,
                            features
                        })?
                    );
                } else {
                    println!("name:          {}", entry.name);
                    println!("path:          {}", entry.path);
                    println!("graph_key:     {}", entry.graph_key);
                    println!("indexed_at:    {}", entry.indexed_at);
                    println!(
                        "git_head:      {}",
                        entry.last_git_head.as_deref().unwrap_or("(unknown)")
                    );
                    println!("stale:         {}", stale);
                    println!("nodes:         {}", entry.stats.nodes);
                    println!("edges:         {}", entry.stats.edges);
                    println!("files:         {}", entry.stats.files);
                    println!("routes:        {}", entry.stats.routes);
                    println!("communities:   {}", entry.stats.communities);
                    println!("processes:     {}", entry.stats.processes);
                    if let Some(fs) = feat_status {
                        println!(
                            "features:      {} ({} nodes, strategy: {})",
                            fs.feature_count, fs.node_count, fs.strategy
                        );
                        println!("pinned:        {}", fs.pinned_count);
                        println!(
                            "feat_version:  {}",
                            &fs.graph_version[..fs.graph_version.len().min(16)]
                        );
                    }
                }
            } else {
                eprintln!(
                    "Registry entry not found for '{name}'. Run `cih-engine analyze <repo>` first."
                );
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Group { command } => match command {
            GroupCommand::Create { name } => cmd::group::run_group_create(&name),
            GroupCommand::Add { name, repo } => cmd::group::run_group_add(&name, &repo),
            GroupCommand::Remove { name, repo } => cmd::group::run_group_remove(&name, &repo),
            GroupCommand::List { json } => cmd::group::run_group_list(json),
            GroupCommand::Sync {
                name,
                falkor_url: _,
                json,
            } => cmd::group::run_group_sync(&name, json),
        },
        Command::Features { command } => match command {
            FeaturesCommand::Show { repo, json } => cmd::features::run_features_show(repo, json),
            FeaturesCommand::Override {
                repo,
                node_id,
                feature,
                reason,
            } => cmd::features::run_features_override(repo, node_id, feature, reason),
        },
        Command::Wiki {
            repo,
            out,
            llm,
            llm_enrich,
            llm_provider,
            llm_provider_config,
            llm_api_key_env,
            evidence,
            llm_base_url,
            llm_model,
            llm_max_tokens,
            llm_timeout_secs,
            llm_retries,
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
        } => wiki::run_wiki(wiki::WikiConfig {
            repo,
            out,
            run_llm: llm || llm_enrich,
            llm: llm::LlmCallConfig {
                provider: llm_provider.parse()?,
                base_url: llm_base_url,
                model: llm_model,
                api_key_env: llm_api_key_env,
                max_tokens: llm_max_tokens,
                timeout_secs: llm_timeout_secs,
                retries: llm_retries,
            },
            llm_provider_config,
            evidence_paths: evidence,
            llm_concurrency,
            llm_debug_evidence,
            llm_dry_run,
            wiki_language,
            wiki_mode: wiki_mode.parse()?,
            grouping: grouping.parse()?,
            html,
            incremental,
            save_evidence,
            filter_community,
            max_communities,
            filter_feature,
            filter_route,
            json,
        }),
        Command::Taint { repo, db, intra_proc, cfg, pdg, json } => cmd::taint::run_taint(
            repo,
            cmd::taint::TaintFlags {
                falkor_url: db.falkor_url,
                graph_key: db.graph_key,
                no_load: db.no_load,
                intra_proc,
                cfg,
                pdg,
                json,
            },
        ),
        Command::Start {
            workspace,
            repo,
            repo_name,
            postgres_password,
            dry_run,
            non_interactive,
        } => start::run_start(start::StartConfig {
            workspace,
            repo,
            repo_name,
            postgres_password,
            dry_run,
            non_interactive,
            ..Default::default()
        }),
        Command::Artifact { command } => run_artifact(command),
        Command::Config { command } => match command {
            ConfigCommand::Decompile { repo } => cmd::config::run_config_decompile(&repo),
        },
        // Handled above before the match; unreachable at runtime.
        Command::Ui => unreachable!(),
    }
}

fn run_artifact(command: ArtifactCommand) -> Result<()> {
    use cih_core::GraphArtifacts;
    match command {
        ArtifactCommand::Export { repo, out } => {
            let cih_dir = repo.join(".cih");
            let artifacts_dir = cih_dir.join("artifacts");
            // Find the latest version dir.
            let version_dir = find_latest_version_dir(&artifacts_dir)?;
            let version_id = version_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            let artifacts = GraphArtifacts {
                nodes_path: version_dir.join("nodes.jsonl"),
                edges_path: version_dir.join("edges.jsonl"),
                version: cih_core::VersionId(version_id.clone()),
            };
            let bundle_path = out.unwrap_or_else(|| cih_dir.join("graph.db.zst"));
            let manifest = artifacts.export_bundle(
                None,
                &cih_dir.join("file-hashes.json"),
                &cih_dir.join("scope.json"),
                &cih_dir.join("repo-map.json"),
                &bundle_path,
            )?;
            println!(
                "Bundle exported to {}: {} files, version {}",
                bundle_path.display(),
                manifest.file_count,
                &manifest.artifact_version[..8.min(manifest.artifact_version.len())]
            );
            Ok(())
        }
        ArtifactCommand::Import { repo, bundle } => {
            let cih_dir = repo.join(".cih");
            let (_, _, manifest) = GraphArtifacts::import_bundle(&bundle, &cih_dir)?;
            println!(
                "Bundle imported: repo={}, {} files, version {}",
                manifest.repo_name,
                manifest.file_count,
                &manifest.artifact_version[..8.min(manifest.artifact_version.len())]
            );
            Ok(())
        }
        ArtifactCommand::Bootstrap {
            repo,
            bundle,
            falkor_url,
            graph_key,
        } => {
            let cih_dir = repo.join(".cih");
            let (artifacts, community, manifest) =
                GraphArtifacts::import_bundle(&bundle, &cih_dir)?;
            println!(
                "Bundle imported: {} files, version {}",
                manifest.file_count,
                &manifest.artifact_version[..8.min(manifest.artifact_version.len())]
            );

            // Bulk-load into FalkorDB.
            let falkor_url = falkor_url.unwrap_or_else(|| DEFAULT_FALKOR_URL.to_string());
            let graph_key = graph_key.unwrap_or_else(|| DEFAULT_GRAPH_KEY.to_string());
            runtime::block_on(async {
                use cih_falkor::FalkorStore;
                use cih_graph_store::GraphStore;
                let store = FalkorStore::connect(&falkor_url, &graph_key)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                store
                    .ensure_schema()
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                store
                    .bulk_load(&artifacts)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                if let Some(comm) = community {
                    store
                        .bulk_load(&comm)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                Ok::<(), anyhow::Error>(())
            })?;

            // Register in registry.
            let root_abs = repo.canonicalize().unwrap_or(repo.clone());
            let registry_path = dirs_next_or_home().join(".cih").join("registry.json");
            let _ = register_repo_in_registry(&registry_path, &root_abs, &artifacts, &graph_key);

            println!("Bootstrap complete. Graph key: {graph_key}");
            Ok(())
        }
    }
}

fn find_latest_version_dir(artifacts_dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(artifacts_dir)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", artifacts_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    entries
        .pop()
        .ok_or_else(|| anyhow::anyhow!("no artifact versions found in {}", artifacts_dir.display()))
}

fn dirs_next_or_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn register_repo_in_registry(
    registry_path: &std::path::Path,
    root: &std::path::Path,
    artifacts: &cih_core::GraphArtifacts,
    graph_key: &str,
) -> Result<()> {
    use cih_core::{Registry, RegistryEntry, RegistryStats};
    let mut registry = if registry_path.exists() {
        let bytes = std::fs::read(registry_path)?;
        serde_json::from_slice::<Registry>(&bytes).unwrap_or_default()
    } else {
        Registry::default()
    };
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let root_str = root.to_string_lossy().to_string();
    let artifacts_dir = artifacts
        .nodes_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let entry = RegistryEntry {
        name: name.clone(),
        path: root_str.clone(),
        graph_key: graph_key.to_string(),
        artifacts_dir,
        community_artifacts_dir: None,
        indexed_at: cih_core::registry::now_rfc3339(),
        last_git_head: None,
        stats: RegistryStats {
            nodes: 0,
            edges: 0,
            files: 0,
            routes: 0,
            communities: 0,
            processes: 0,
        },
    };
    registry.entries.retain(|r| r.path != root_str);
    registry.entries.push(entry);
    if let Some(parent) = registry_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&registry).map_err(|e| anyhow::anyhow!("{e}"))?;
    std::fs::write(registry_path, json)?;
    println!("Registered repo '{}' in registry.", name);
    Ok(())
}

// ── CLI argument parse tests ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::path::PathBuf;

    /// Parsing `analyze /tmp/repo --all` should set repo to Some("/tmp/repo").
    #[test]
    fn test_analyze_explicit_repo() {
        let result = Cli::try_parse_from(["cih-engine", "analyze", "/tmp/repo", "--all"]);
        assert!(result.is_ok(), "unexpected parse failure: {result:?}");
        let cli = result.unwrap();
        match cli.command {
            Command::Analyze { repo, .. } => {
                assert_eq!(repo, Some(PathBuf::from("/tmp/repo")));
            }
            other => panic!("expected Analyze command, got {other:?}"),
        }
    }

    /// Parsing `analyze --all` (no repo) should keep repo as None (cwd fallback at runtime).
    #[test]
    fn test_analyze_omitted_repo() {
        let result = Cli::try_parse_from(["cih-engine", "analyze", "--all"]);
        assert!(result.is_ok(), "unexpected parse failure: {result:?}");
        let cli = result.unwrap();
        match cli.command {
            Command::Analyze { repo, .. } => {
                assert_eq!(repo, None, "repo should be None when omitted, got {repo:?}");
            }
            other => panic!("expected Analyze command, got {other:?}"),
        }
    }

    /// Parsing `analyze` (no repo, no --all) should succeed — scope gate is a runtime check.
    #[test]
    fn test_analyze_no_repo_and_no_scope() {
        let result = Cli::try_parse_from(["cih-engine", "analyze"]);
        assert!(result.is_ok(), "unexpected parse failure: {result:?}");
        let cli = result.unwrap();
        match cli.command {
            Command::Analyze { repo, .. } => {
                assert_eq!(repo, None, "repo should be None when omitted, got {repo:?}");
            }
            other => panic!("expected Analyze command, got {other:?}"),
        }
    }
}
