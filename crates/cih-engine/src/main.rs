mod analyze;
mod db;
mod discover;
mod embed;
mod file_cache;
mod scan;
mod scope;
#[cfg(test)]
mod tests;
mod versioning;

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
    }
}
