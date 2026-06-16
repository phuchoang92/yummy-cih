mod analyze;
mod db;
mod discover;
mod embed;
mod file_cache;
mod group;
mod group_cmd;
mod llm;
mod registry;
mod scan;
mod scope;
#[cfg(test)]
mod tests;
mod versioning;
mod wiki_cmd;

use std::path::PathBuf;

use analyze::AnalyzeFlags;
use anyhow::Result;
use clap::{Parser, Subcommand};

/// Default FalkorDB URL (Homebrew redis squats 6379, FalkorDB on 6380).
const DEFAULT_FALKOR_URL: &str = "redis://127.0.0.1:6380";
const DEFAULT_GRAPH_KEY: &str = "cih";

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
        repo: PathBuf,
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
        /// FalkorDB URL. Defaults to $FALKOR_URL or redis://127.0.0.1:6380.
        #[arg(long, env = "FALKOR_URL")]
        falkor_url: Option<String>,
        /// FalkorDB graph key. Defaults to $CIH_GRAPH_KEY or "cih".
        #[arg(long, env = "CIH_GRAPH_KEY")]
        graph_key: Option<String>,
        /// Skip the FalkorDB load step (emit JSONL artifacts only).
        #[arg(long)]
        no_load: bool,
        /// Disable incremental parse cache and re-parse all files.
        #[arg(long)]
        no_cache: bool,
    },
    /// Re-run the resolve pass using the saved scope (.cih/scope.json), without re-scanning.
    /// Useful when the resolver changes but the source files have not.
    Resolve {
        /// Repository root (must contain .cih/scope.json from a prior `analyze` run).
        repo: PathBuf,
        /// FalkorDB URL. Defaults to $FALKOR_URL or redis://127.0.0.1:6380.
        #[arg(long, env = "FALKOR_URL")]
        falkor_url: Option<String>,
        /// FalkorDB graph key. Defaults to $CIH_GRAPH_KEY or "cih".
        #[arg(long, env = "CIH_GRAPH_KEY")]
        graph_key: Option<String>,
        /// Skip the FalkorDB load step (emit JSONL artifacts only).
        #[arg(long)]
        no_load: bool,
        /// Print the summary as JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
    /// Detect communities and process traces from the latest analyzed artifacts.
    Discover {
        /// Repository root with `.cih/artifacts/<version>` from a prior analyze/resolve run.
        repo: PathBuf,
        /// FalkorDB URL. Defaults to $FALKOR_URL or redis://127.0.0.1:6380.
        #[arg(long, env = "FALKOR_URL")]
        falkor_url: Option<String>,
        /// FalkorDB graph key. Defaults to $CIH_GRAPH_KEY or "cih".
        #[arg(long, env = "CIH_GRAPH_KEY")]
        graph_key: Option<String>,
        /// Skip the FalkorDB load step (emit JSONL artifacts only).
        #[arg(long)]
        no_load: bool,
        /// Print the summary as JSON instead of the human summary.
        #[arg(long)]
        json: bool,
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
        /// Enable LLM enrichment using an OpenAI-compatible API.
        /// Reads CIH_LLM_API_KEY, OPENAI_API_KEY, or ANTHROPIC_API_KEY from the environment.
        #[arg(long, env = "CIH_LLM")]
        llm: bool,
        /// Deprecated: alias for --llm. Will be removed in a future release.
        #[arg(long, env = "CIH_LLM_ENRICH", hide = true)]
        llm_enrich: bool,
        /// LLM provider adapter.
        #[arg(long, default_value = "openai-compatible")]
        llm_provider: String,
        /// JSON config file for --llm-provider http-json.
        #[arg(long)]
        llm_provider_config: Option<PathBuf>,
        /// Explicit API key environment variable for the selected provider.
        #[arg(long)]
        llm_api_key_env: Option<String>,
        /// External evidence file (.md or .txt) to include in LLM wiki prompts.
        #[arg(long = "evidence")]
        evidence: Vec<PathBuf>,
        /// OpenAI-compatible API base URL.
        #[arg(long, default_value = "https://api.openai.com/v1")]
        llm_base_url: String,
        /// Model name for LLM enrichment.
        #[arg(long, default_value = "gpt-4o-mini")]
        llm_model: String,
        /// Maximum output tokens per LLM call.
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
        /// Print outcome as JSON instead of the human summary.
        #[arg(long)]
        json: bool,
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

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
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
            falkor_url,
            graph_key,
            no_load,
            no_cache,
        } => analyze::run_analyze(
            repo,
            AnalyzeFlags {
                all,
                modules,
                include,
                exclude,
                include_decompiled,
                scope,
                json,
                falkor_url,
                graph_key,
                no_load,
                no_cache,
            },
        ),
        Command::Resolve {
            repo,
            falkor_url,
            graph_key,
            no_load,
            json,
        } => analyze::run_resolve(repo, falkor_url, graph_key, no_load, json),
        Command::Discover {
            repo,
            falkor_url,
            graph_key,
            no_load,
            json,
        } => discover::run_discover(repo, falkor_url, graph_key, no_load, json),
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
                if json {
                    #[derive(serde::Serialize)]
                    struct StatusOutput<'a> {
                        entry: &'a cih_core::RegistryEntry,
                        stale: bool,
                    }
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&StatusOutput { entry, stale })?
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
            GroupCommand::Create { name } => group_cmd::run_group_create(&name),
            GroupCommand::Add { name, repo } => group_cmd::run_group_add(&name, &repo),
            GroupCommand::Remove { name, repo } => group_cmd::run_group_remove(&name, &repo),
            GroupCommand::List { json } => group_cmd::run_group_list(json),
            GroupCommand::Sync {
                name,
                falkor_url: _,
                json,
            } => group_cmd::run_group_sync(&name, json),
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
            json,
        } => wiki_cmd::run_wiki(
            &repo,
            out,
            llm || llm_enrich,
            &llm_provider,
            llm_provider_config,
            llm_api_key_env,
            evidence,
            &llm_base_url,
            &llm_model,
            llm_max_tokens,
            llm_timeout_secs,
            llm_retries,
            llm_concurrency,
            llm_debug_evidence,
            llm_dry_run,
            &wiki_language,
            json,
        ),
    }
}
