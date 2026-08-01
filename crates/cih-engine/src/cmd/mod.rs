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
pub mod refresh;
pub mod start;
pub mod start_env;
pub mod status;
pub mod taint;
pub mod tui;
pub mod wiki;

use anyhow::Result;
use clap::Parser;

use crate::runtime;
use args::{ArtifactCommand, Cli, Command, ConfigCommand, DbArgs, FeaturesCommand, GroupCommand};

/// Product-level defaults injected before command dispatch. Explicit command
/// values always win, so compatibility binaries retain their environment and
/// CLI behavior while the portable product can select Ladybug deterministically.
#[derive(Clone, Debug, Default)]
pub struct ProductDefaults {
    pub backend: Option<String>,
    pub store_url: Option<String>,
    pub graph_key: Option<String>,
}

impl ProductDefaults {
    fn apply(&self, command: &mut Command) {
        let apply_db = |db: &mut DbArgs| {
            if db.backend.is_none() {
                db.backend.clone_from(&self.backend);
            }
            if db.falkor_url.is_none() {
                db.falkor_url.clone_from(&self.store_url);
            }
            if db.graph_key.is_none() {
                db.graph_key.clone_from(&self.graph_key);
            }
        };
        match command {
            Command::Analyze(args) => apply_db(&mut args.db),
            Command::Resolve { db, .. } => apply_db(db),
            Command::Discover(args) => apply_db(&mut args.db),
            Command::Refresh(args) => apply_db(&mut args.db),
            Command::Taint(args) => apply_db(&mut args.db),
            Command::Artifact {
                command:
                    ArtifactCommand::Bootstrap {
                        backend,
                        falkor_url,
                        graph_key,
                        ..
                    },
            } => {
                if backend.is_none() {
                    backend.clone_from(&self.backend);
                }
                if falkor_url.is_none() {
                    falkor_url.clone_from(&self.store_url);
                }
                if graph_key.is_none() {
                    graph_key.clone_from(&self.graph_key);
                }
            }
            _ => {}
        }
    }
}

/// Binary entry point: tracing + runtime init, parse, dispatch.
pub fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
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

    dispatch(cli.command, ProductDefaults::default())
}

/// Execute one already-parsed engine command. This entry point does not
/// initialize tracing or create a Tokio runtime; the product binary owns those
/// process-wide concerns.
pub fn dispatch(mut command: Command, defaults: ProductDefaults) -> Result<()> {
    defaults.apply(&mut command);
    match command {
        Command::Scan { repo, json } => crate::scan::run_scan(&repo, json),
        Command::Analyze(a) => analyze::run(a),
        Command::Resolve { repo, db, json } => crate::analyze::run_resolve(
            repo,
            db.backend,
            db.falkor_url,
            db.graph_key,
            db.no_load,
            json,
        ),
        Command::Discover(a) => discover::run(a),
        #[cfg(feature = "semantic")]
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
            GroupCommand::Status { name, json } => group::run_group_status(&name, json),
        },
        Command::Wiki(a) => wiki::run(a),
        Command::Refresh(a) => refresh::run(a),
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
                backend: a.db.backend,
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
