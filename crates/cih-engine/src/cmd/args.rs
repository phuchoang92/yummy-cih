//! The complete clap surface of `cih-engine`: root parser, subcommand enum,
//! and per-command argument structs. Dispatch lives in [`super::main`];
//! command bodies live in the sibling `cmd::*` modules.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Shared FalkorDB connection + load options, used by Analyze, Resolve, and Discover.
#[derive(Debug, clap::Args)]
pub struct DbArgs {
    /// FalkorDB URL. Defaults to $FALKOR_URL or redis://127.0.0.1:6380.
    #[arg(long, env = "FALKOR_URL")]
    pub falkor_url: Option<String>,
    /// FalkorDB graph key. Defaults to $CIH_GRAPH_KEY or "cih".
    #[arg(long, env = "CIH_GRAPH_KEY")]
    pub graph_key: Option<String>,
    /// Skip the FalkorDB load step (emit JSONL artifacts only).
    #[arg(long)]
    pub no_load: bool,
}

#[derive(Debug, Parser)]
#[command(name = "cih-engine", about = "Code Intelligence Hub engine CLI")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Fast repository discovery pass. Writes .cih/repo-map.json.
    Scan {
        /// Repository root to scan.
        repo: PathBuf,
        /// Print RepoMap JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// Parse selected files, emit structure graph, and load into FalkorDB.
    Analyze(AnalyzeArgs),
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
    Discover(DiscoverArgs),
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
    Wiki(WikiArgs),
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
    Taint(TaintArgs),
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
    Start(StartArgs),
    /// Export, import, or bootstrap CIH bundle archives (Gap 5).
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ArtifactCommand {
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
pub enum ConfigCommand {
    /// Show effective settings for a repo, annotated with the layer each value
    /// came from (default / ~/.cih/config.toml / cih.toml).
    Show {
        /// Repository root. Reads `<repo>/cih.toml` and `~/.cih/config.toml`.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Write a commented starter settings file with every option at its default.
    Init {
        /// Repository root. Writes `<repo>/cih.toml` (unless --global).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Write `~/.cih/config.toml` (cross-repo defaults) instead of `<repo>/cih.toml`.
        #[arg(long)]
        global: bool,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Interactively edit decompile settings (JAR directories, prefixes, tool path).
    Decompile {
        /// Repository root. Reads and writes `<repo>/cih.decompile.toml`.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum GroupCommand {
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
pub enum FeaturesCommand {
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
    /// LLM-review the current embedding clusters and auto-write pin overrides for weakly-assigned
    /// nodes (e.g. a utility mis-clustered into the wrong feature, or a boundary node left in
    /// `shared`). Writes `.cih/feature-overrides.json`; re-run `discover` to apply.
    /// Use `--dry-run` to preview without writing.
    Review {
        /// Repository root with `.cih/artifacts-features/` from a prior discover run.
        repo: PathBuf,
        /// LLM provider: deepseek, gemini, anthropic, bedrock, openai-compatible, http-json.
        #[arg(long)]
        llm_provider: String,
        /// LLM model. Empty uses the provider default (e.g. claude-haiku-4-5-20251001 for anthropic).
        #[arg(long, default_value = "")]
        llm_model: String,
        /// Base URL for --llm-provider openai-compatible.
        #[arg(long)]
        llm_base_url: Option<String>,
        /// Env var holding the API key. Defaults to auto-detect (ANTHROPIC_API_KEY, etc.).
        #[arg(long)]
        llm_api_key_env: Option<String>,
        /// Max output tokens per LLM call.
        #[arg(long, default_value_t = 2048)]
        llm_max_tokens: u32,
        /// Timeout in seconds per LLM call.
        #[arg(long, default_value_t = 60)]
        llm_timeout_secs: u64,
        /// Preview proposed overrides without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Maximum candidate nodes to review (0 = all).
        #[arg(long, default_value_t = 0)]
        limit: usize,
        /// Also review the lowest-confidence *in-cluster* members, not just `shared`/boundary nodes.
        #[arg(long)]
        include_weak_members: bool,
        /// Minimum LLM confidence to accept a reassignment (0.0–1.0).
        #[arg(long, default_value_t = 0.7)]
        min_confidence: f32,
    },
}

#[derive(Debug, clap::Args)]
pub struct AnalyzeArgs {
    /// Repository root to analyze.
    pub repo: Option<PathBuf>,
    /// Select all Java files, excluding decompiled dirs unless requested.
    #[arg(long)]
    pub all: bool,
    /// Select one or more module names, comma-delimited or repeated.
    #[arg(long = "module", value_delimiter = ',')]
    pub modules: Vec<String>,
    /// Include Java files matching this repo-relative glob. Can be repeated.
    #[arg(long)]
    pub include: Vec<String>,
    /// Exclude Java files matching this repo-relative glob. Can be repeated.
    #[arg(long)]
    pub exclude: Vec<String>,
    /// Include files under decompiled dirs such as .workspace-dependencies.
    #[arg(long)]
    pub include_decompiled: bool,
    /// Scope TOML file. Defaults to `<repo>/cih.scope.toml` when present.
    #[arg(long)]
    pub scope: Option<PathBuf>,
    /// Print the resolved ScopeFile JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub db: DbArgs,
    /// Disable incremental parse cache and re-parse all files.
    #[arg(long)]
    pub no_cache: bool,
    /// Skip integration and DI XML extraction (faster on large repos).
    #[arg(long)]
    pub skip_xml_integration: bool,
    /// Limit analysis to these languages (comma-delimited or repeated). Default: all.
    /// Example: --language java,typescript
    #[arg(long = "language", value_delimiter = ',')]
    pub languages: Vec<String>,
    /// CXF servlet base path (e.g. /rest) prepended to <jaxrs:server> route paths.
    /// Overrides auto-detection. Default: cih.toml `cxf_base_path`, else auto-detect.
    #[arg(long)]
    pub cxf_base_path: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct DiscoverArgs {
    /// Repository root with `.cih/artifacts/<version>` from a prior analyze/resolve run.
    pub repo: PathBuf,
    #[command(flatten)]
    pub db: DbArgs,
    /// Print the summary as JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,

    // ── Community detection overrides ──────────────────────────────────
    /// Community detection strategy.
    /// "package" (default): groups by package/module structure — one community per feature.
    /// "graph": Leiden graph-clustering — groups by call-graph connectivity.
    /// Falls back to cih.toml [discover].community_strategy, then "package".
    #[arg(long)]
    pub community_strategy: Option<String>,
    /// Leiden resolution (only used with --community-strategy graph).
    /// Higher = more, smaller communities; lower = fewer, larger ones. Default: 1.0.
    #[arg(long)]
    pub resolution: Option<f64>,
    /// Minimum community size (only used with --community-strategy graph).
    /// Drop communities smaller than this many members. Default: 2 (3 for large graphs).
    #[arg(long)]
    pub min_community_size: Option<usize>,

    // ── Process trace overrides ────────────────────────────────────────
    /// Maximum BFS depth per process trace. Default: 10.
    #[arg(long)]
    pub max_trace_depth: Option<usize>,
    /// Maximum number of processes kept after deduplication. Default: scales with codebase size.
    #[arg(long)]
    pub max_processes: Option<usize>,
    /// Maximum call-graph branching factor per BFS step. Default: 4.
    #[arg(long)]
    pub max_branching: Option<usize>,
    /// Minimum edge confidence to follow during BFS (0.0–1.0). Default: 0.5.
    #[arg(long)]
    pub min_trace_confidence: Option<f32>,

    // ── Feature grouping strategy ──────────────────────────────────────
    /// Feature classification strategy: package (default), structural, hybrid, llm, embed.
    /// "hybrid" runs structural → package → embed-filler → llm (if --feature-llm-provider set).
    /// "llm" requires --feature-llm-provider.
    /// "embed" clusters by semantic similarity (k-NN + Leiden); requires --pg-url and a prior `cih embed`.
    /// Falls back to cih.toml [discover].feature_strategy, then "package".
    #[arg(long)]
    pub feature_strategy: Option<String>,
    /// LLM provider for feature classification.
    /// One of: deepseek, gemini, anthropic, bedrock, openai-compatible.
    /// Required when --feature-strategy is llm or hybrid with LLM stage.
    #[arg(long)]
    pub feature_llm_provider: Option<String>,
    /// LLM model for feature classification.
    /// Defaults: deepseek-chat (deepseek), gemini-2.5-flash (gemini),
    /// claude-haiku-4-5-20251001 (anthropic), us.anthropic.claude-haiku-4-5-20251001 (bedrock),
    /// gpt-4o-mini (openai-compatible).
    #[arg(long)]
    pub feature_llm_model: Option<String>,
    /// Base URL for --feature-llm-provider openai-compatible.
    #[arg(long)]
    pub feature_llm_base_url: Option<String>,
    /// Override which env var holds the API key for feature LLM.
    /// Defaults to auto-detect (CIH_LLM_API_KEY, DEEPSEEK_API_KEY, etc.).
    #[arg(long)]
    pub feature_llm_api_key_env: Option<String>,
    /// Maximum output tokens per feature LLM call.
    #[arg(long)]
    pub feature_llm_max_tokens: Option<u32>,
    /// Timeout in seconds per feature LLM API call.
    #[arg(long)]
    pub feature_llm_timeout_secs: Option<u64>,

    // ── Embedding feature clustering (--feature-strategy embed) ────────
    /// Postgres URL for --feature-strategy embed. Defaults to $CIH_PG_URL.
    /// Requires a prior `cih embed` against the same repo.
    #[arg(long, env = "CIH_PG_URL")]
    pub pg_url: Option<String>,
    /// embed strategy: minimum cosine similarity for a k-NN edge (0.0–1.0). Default: 0.65.
    #[arg(long)]
    pub embed_similarity_threshold: Option<f32>,
    /// embed strategy: number of nearest neighbors per node. Default: 15.
    #[arg(long)]
    pub embed_knn: Option<usize>,
    /// embed strategy: Leiden resolution — higher = more, smaller clusters. Default: 0.8.
    #[arg(long)]
    pub embed_leiden_resolution: Option<f64>,
}

#[derive(Debug, clap::Args)]
pub struct WikiArgs {
    /// Repository root with `.cih/artifacts/` and `.cih/artifacts-community/` from prior runs.
    pub repo: PathBuf,
    /// Output directory. Defaults to `<repo>/.cih/wiki`.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Enable LLM enrichment. Set an API key env var before using:
    /// DEEPSEEK_API_KEY, GEMINI_API_KEY, OPENAI_API_KEY, ANTHROPIC_API_KEY, AWS_BEARER_TOKEN_BEDROCK, or CIH_LLM_API_KEY.
    #[arg(long, env = "CIH_LLM")]
    pub llm: bool,
    /// Deprecated: alias for --llm. Will be removed in a future release.
    #[arg(long, env = "CIH_LLM_ENRICH", hide = true)]
    pub llm_enrich: bool,
    /// LLM provider adapter. One of:
    ///   deepseek          — DeepSeek API (DEEPSEEK_API_KEY, model: deepseek-chat)
    ///   gemini            — Google Gemini API (GEMINI_API_KEY, model: gemini-2.5-flash)
    ///   anthropic         — Anthropic API (ANTHROPIC_API_KEY, model: claude-haiku-4-5-20251001)
    ///   bedrock           — AWS Bedrock Converse API (AWS_BEARER_TOKEN_BEDROCK, model: us.anthropic.claude-haiku-4-5-20251001)
    ///   openai-compatible — Any OpenAI-compatible endpoint (use with --llm-base-url)
    ///   http-json         — Custom HTTP adapter (use with --llm-provider-config)
    /// Falls back to cih.toml [wiki].llm_provider, then "openai-compatible".
    #[arg(long)]
    pub llm_provider: Option<String>,
    /// JSON config file for --llm-provider http-json.
    #[arg(long)]
    pub llm_provider_config: Option<PathBuf>,
    /// Override which env var holds the API key. Defaults to auto-detect from provider.
    #[arg(long)]
    pub llm_api_key_env: Option<String>,
    /// External evidence file (.md or .txt) to include in LLM wiki prompts.
    #[arg(long = "evidence")]
    pub evidence: Vec<PathBuf>,
    /// Base URL for --llm-provider openai-compatible. Ignored for deepseek/gemini/anthropic.
    #[arg(long)]
    pub llm_base_url: Option<String>,
    /// Model name for LLM enrichment. Provider-specific defaults:
    ///   deepseek-chat (deepseek), gemini-2.5-flash (gemini),
    ///   claude-haiku-4-5-20251001 (anthropic), gpt-4o-mini (openai-compatible).
    #[arg(long)]
    pub llm_model: Option<String>,
    /// Maximum output tokens per LLM call. Increase to 4096 for Gemini to avoid truncation.
    #[arg(long)]
    pub llm_max_tokens: Option<u32>,
    /// Timeout in seconds per LLM API call.
    #[arg(long)]
    pub llm_timeout_secs: Option<u64>,
    /// Retries on transient LLM failures.
    #[arg(long)]
    pub llm_retries: Option<u32>,
    /// Maximum concurrent LLM calls.
    #[arg(long)]
    pub llm_concurrency: Option<usize>,
    /// Print evidence packs to stdout instead of calling the LLM.
    #[arg(long)]
    pub llm_debug_evidence: bool,
    /// Print prompts to stdout without calling the LLM (dry run).
    #[arg(long)]
    pub llm_dry_run: bool,
    /// Documentation language for LLM-generated text.
    #[arg(long)]
    pub wiki_language: Option<String>,
    /// Wiki generation mode: graph (no LLM), llm-summary, or llm-full.
    #[arg(long)]
    pub wiki_mode: Option<String>,
    /// Community grouping strategy: package (by Java package path, default), graph (Leiden communities), or llm (LLM-proposed).
    #[arg(long)]
    pub grouping: Option<String>,
    /// Write a standalone index.html viewer alongside the Markdown files.
    #[arg(long)]
    pub html: bool,
    /// Skip communities whose evidence hash has not changed since the last run.
    #[arg(long)]
    pub incremental: bool,
    /// Save per-community evidence packs to .cih/wiki/evidence/<slug>.json.
    #[arg(long = "save-evidence")]
    pub save_evidence: bool,
    /// Only generate docs for communities whose name contains this substring (case-insensitive).
    /// Can be specified multiple times to include several groups.
    #[arg(long = "filter-community")]
    pub filter_community: Vec<String>,
    /// Only generate pages for features (module directories) whose name contains this substring
    /// (case-insensitive). Works with or without a prior `discover` run.
    /// Can be specified multiple times.
    #[arg(long = "filter-feature")]
    pub filter_feature: Vec<String>,
    /// Only generate pages for communities that own at least one route whose path starts
    /// with or contains this pattern (case-insensitive).
    /// Can be specified multiple times to match several prefixes.
    /// Example: --filter-route /api/payment --filter-route /api/order
    #[arg(long = "filter-route")]
    pub filter_route: Vec<String>,
    /// Limit total number of communities processed (useful for quick tests).
    #[arg(long)]
    pub max_communities: Option<usize>,
    /// Print outcome as JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct TaintArgs {
    /// Repository root with `.cih/artifacts/` from a prior analyze run.
    pub repo: PathBuf,
    #[command(flatten)]
    pub db: DbArgs,
    /// Disable intra-procedural liveness analysis (faster, more false positives).
    #[arg(long = "no-intra-proc", default_value_t = true, action = clap::ArgAction::SetFalse)]
    pub intra_proc: bool,
    /// Disable CFG construction and dominance-tree analysis.
    #[arg(long = "no-cfg", default_value_t = true, action = clap::ArgAction::SetFalse)]
    pub cfg: bool,
    /// Disable PDG-based flow-sensitive taint analysis.
    #[arg(long = "no-pdg", default_value_t = true, action = clap::ArgAction::SetFalse)]
    pub pdg: bool,
    /// Print results as JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct StartArgs {
    /// CIH workspace directory containing docker-compose.yml. Default: current directory.
    #[arg(long, default_value = ".")]
    pub workspace: PathBuf,
    /// Target Java/Spring repository path. Required when --non-interactive.
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Repository name for docs viewer URL prefix. Default: derived from repo path.
    #[arg(long)]
    pub repo_name: Option<String>,
    /// Postgres password written to .env. Required in --non-interactive mode
    /// (or read from the POSTGRES_PASSWORD env var).
    #[arg(long)]
    pub postgres_password: Option<String>,
    /// Print plan without writing files or running commands.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip interactive prompts (requires --repo).
    #[arg(long)]
    pub non_interactive: bool,
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
        match result.unwrap().command {
            Command::Analyze(args) => {
                assert_eq!(args.repo, Some(PathBuf::from("/tmp/repo")));
            }
            other => panic!("expected Analyze command, got {other:?}"),
        }
    }

    /// Parsing `analyze --all` (no repo) should keep repo as None (cwd fallback at runtime).
    #[test]
    fn test_analyze_omitted_repo() {
        let result = Cli::try_parse_from(["cih-engine", "analyze", "--all"]);
        assert!(result.is_ok(), "unexpected parse failure: {result:?}");
        match result.unwrap().command {
            Command::Analyze(args) => {
                assert_eq!(args.repo, None, "repo should be None when omitted");
            }
            other => panic!("expected Analyze command, got {other:?}"),
        }
    }

    /// Parsing `analyze` (no repo, no --all) should succeed — scope gate is a runtime check.
    #[test]
    fn test_analyze_no_repo_and_no_scope() {
        let result = Cli::try_parse_from(["cih-engine", "analyze"]);
        assert!(result.is_ok(), "unexpected parse failure: {result:?}");
        match result.unwrap().command {
            Command::Analyze(args) => {
                assert_eq!(args.repo, None, "repo should be None when omitted");
            }
            other => panic!("expected Analyze command, got {other:?}"),
        }
    }
}
