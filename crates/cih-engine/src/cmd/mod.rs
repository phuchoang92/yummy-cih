//! The CLI layer: clap surface ([`args`]), the dispatch entry point
//! ([`main`]), and one module per command (family). Command modules resolve
//! layered settings and call into the pipeline/library modules; every
//! dispatch arm below stays a single call.

pub mod args;

pub mod analyze;
pub mod artifact;
pub mod config;
pub mod discover;
pub mod features;
pub mod group;
pub mod group_sync;
pub mod list;
pub mod start;
pub mod start_env;
pub mod status;
pub mod taint;
pub mod tui;
pub mod wiki;

use anyhow::Result;
use clap::Parser;

use crate::runtime;
use args::{Cli, Command, ConfigCommand, FeaturesCommand, GroupCommand};

/// Binary entry point: tracing + runtime init, parse, dispatch.
pub fn main() -> Result<()> {
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
        if let Some(cmd_args) = tui::run_tui()? {
            let cmd_display = std::iter::once("cih-engine")
                .chain(cmd_args.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            println!();
            println!("  Running: {}", cmd_display);
            println!();
            let exe =
                std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("cih-engine"));
            let status = std::process::Command::new(&exe).args(&cmd_args).status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
        return Ok(());
    }

    match cli.command {
        Command::Scan { repo, json } => crate::scan::run_scan(&repo, json),
        Command::Analyze(a) => analyze::run(a),
        Command::Resolve { repo, db, json } => {
            crate::analyze::run_resolve(repo, db.falkor_url, db.graph_key, db.no_load, json)
        }
        Command::Discover(a) => discover::run(a),
        Command::Embed {
            repo,
            pg_url,
            model,
            json,
        } => crate::embed::run_embed(repo, pg_url, model, json),
        Command::List { json } => list::run(json),
        Command::Status { name, json } => status::run(name, json),
        Command::Group { command } => match command {
            GroupCommand::Create { name } => group::run_group_create(&name),
            GroupCommand::Add { name, repo } => group::run_group_add(&name, &repo),
            GroupCommand::Remove { name, repo } => group::run_group_remove(&name, &repo),
            GroupCommand::List { json } => group::run_group_list(json),
            GroupCommand::Sync {
                name,
                falkor_url: _,
                json,
            } => group::run_group_sync(&name, json),
        },
        Command::Wiki(a) => wiki::run(a),
        Command::Features { command } => match command {
            FeaturesCommand::Show { repo, json } => features::run_features_show(repo, json),
            FeaturesCommand::Override {
                repo,
                node_id,
                feature,
                reason,
            } => features::run_features_override(repo, node_id, feature, reason),
            FeaturesCommand::Review {
                repo,
                llm_provider,
                llm_model,
                llm_base_url,
                llm_api_key_env,
                llm_max_tokens,
                llm_timeout_secs,
                dry_run,
                limit,
                include_weak_members,
                min_confidence,
            } => features::run_features_review(features::ReviewFlags {
                repo,
                provider: llm_provider,
                model: llm_model,
                base_url: llm_base_url,
                api_key_env: llm_api_key_env,
                max_tokens: llm_max_tokens,
                timeout_secs: llm_timeout_secs,
                dry_run,
                limit: if limit == 0 { None } else { Some(limit) },
                include_weak_members,
                min_confidence,
            }),
        },
        Command::Taint(a) => taint::run_taint(
            a.repo,
            taint::TaintFlags {
                falkor_url: a.db.falkor_url,
                graph_key: a.db.graph_key,
                no_load: a.db.no_load,
                intra_proc: a.intra_proc,
                cfg: a.cfg,
                pdg: a.pdg,
                json: a.json,
            },
        ),
        Command::Start(a) => start::run_start(start::StartConfig {
            workspace: a.workspace,
            repo: a.repo,
            repo_name: a.repo_name,
            postgres_password: a.postgres_password,
            dry_run: a.dry_run,
            non_interactive: a.non_interactive,
            ..Default::default()
        }),
        Command::Artifact { command } => artifact::run(command),
        Command::Config { command } => match command {
            ConfigCommand::Show { repo, json } => config::run_config_show(&repo, json),
            ConfigCommand::Init {
                repo,
                global,
                force,
            } => config::run_config_init(&repo, global, force),
            ConfigCommand::Decompile { repo } => config::run_config_decompile(&repo),
        },
        // Handled above before the match; unreachable at runtime.
        Command::Ui => unreachable!(),
    }
}
