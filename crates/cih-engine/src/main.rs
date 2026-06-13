mod scan;
mod scope;

use std::path::PathBuf;
use std::process;

use anyhow::Result;
use clap::{Parser, Subcommand};
use scope::ScopeRequest;

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
    /// Resolve and persist the Java files selected for a future analyze run.
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
        /// Scope TOML file. Defaults to <repo>/cih.scope.toml when present.
        #[arg(long)]
        scope: Option<PathBuf>,
        /// Print the resolved ScopeFile JSON instead of the human summary.
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
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
        } => run_analyze(
            repo,
            AnalyzeFlags {
                all,
                modules,
                include,
                exclude,
                include_decompiled,
                scope,
                json,
            },
        ),
    }
}

#[derive(Debug)]
struct AnalyzeFlags {
    all: bool,
    modules: Vec<String>,
    include: Vec<String>,
    exclude: Vec<String>,
    include_decompiled: bool,
    scope: Option<PathBuf>,
    json: bool,
}

fn run_analyze(repo: PathBuf, flags: AnalyzeFlags) -> Result<()> {
    let scan = scan::scan_repo(&repo)?;
    let repo_map_path = scan::write_repo_map(&scan.repo_map)?;
    let request = build_scope_request(&repo, &flags)?;

    if !request.has_selector() {
        scan::print_summary(&scan.repo_map, &repo_map_path);
        println!();
        println!("Choose a scope: --all | --module <names> | --include <glob> | a cih.scope.toml");
        process::exit(2);
    }

    let scope_file = scope::resolve(&scan.repo_map, &scan.java_files, request)?;
    let output_path = scope::write_scope_file(&scope_file)?;

    if flags.json {
        println!("{}", serde_json::to_string_pretty(&scope_file)?);
    } else {
        println!(
            "Scope: {} .java files across {} modules -> {} (parse/load lands in Task 5/6).",
            scope_file.file_count,
            scope_file.modules.len(),
            output_path.display()
        );
    }
    Ok(())
}

fn build_scope_request(repo: &std::path::Path, flags: &AnalyzeFlags) -> Result<ScopeRequest> {
    let scope_path = if let Some(path) = &flags.scope {
        Some(path.clone())
    } else {
        let default = repo.join("cih.scope.toml");
        default.exists().then_some(default)
    };

    let mut request = if let Some(path) = scope_path {
        ScopeRequest::from_toml(&path)?
    } else {
        ScopeRequest::default()
    };

    if flags.all {
        request.all = true;
        request.modules.clear();
        request.include.clear();
    } else if !flags.modules.is_empty() {
        request.all = false;
        request.modules = flags.modules.clone();
        request.include.clear();
    } else if !flags.include.is_empty() {
        request.all = false;
        request.modules.clear();
        request.include = flags.include.clone();
    }

    if !flags.exclude.is_empty() {
        request.exclude = flags.exclude.clone();
    }
    if flags.include_decompiled {
        request.include_decompiled = true;
    }

    Ok(request)
}
