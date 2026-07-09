//! `cih config` — view (`show`), scaffold (`init`), and edit (`decompile`) settings.

use std::path::Path;

use anyhow::{bail, Result};
use dialoguer::{theme::ColorfulTheme, Input, Select};

use crate::decompile_config::{DecompileConfig, DecompileSource};
use crate::settings::{self, Layers};

/// `cih config show` — print the effective settings for a repo, annotating each
/// value with the layer it came from (default / ~/.cih/config.toml / cih.toml).
pub fn run_config_show(repo: &Path, json: bool) -> Result<()> {
    let layers = Layers::load(repo);
    let rows = settings::effective_rows(&layers);

    if json {
        let obj: serde_json::Map<String, serde_json::Value> = rows
            .iter()
            .map(|r| {
                (
                    format!("{}.{}", r.section, r.key),
                    serde_json::json!({ "value": r.value, "source": r.source.label() }),
                )
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&obj)?);
        return Ok(());
    }

    println!("\n  CIH — effective settings");
    println!("  ─────────────────────────────────");
    println!("  repo:  {}", settings::repo_config_path(repo).display());
    match settings::home_config_path() {
        Some(p) => println!("  home:  {}\n", p.display()),
        None => println!("  home:  (HOME unset)\n"),
    }

    let key_w = rows.iter().map(|r| r.key.len()).max().unwrap_or(0);
    let val_w = rows.iter().map(|r| r.value.len()).max().unwrap_or(0);
    let mut current = "";
    for r in &rows {
        if r.section != current {
            println!("  [{}]", r.section);
            current = r.section;
        }
        println!(
            "    {:<key_w$}  {:<val_w$}  ({})",
            r.key,
            r.value,
            r.source.label(),
            key_w = key_w,
            val_w = val_w,
        );
    }
    println!("\n  Precedence: flag > env > cih.toml > ~/.cih/config.toml > default");
    println!("  Edit cih.toml (or run `cih config init`) to change these.\n");
    Ok(())
}

/// `cih config init` — write a commented starter settings file. Writes
/// `<repo>/cih.toml` by default, or `~/.cih/config.toml` with `global`.
pub fn run_config_init(repo: &Path, global: bool, force: bool) -> Result<()> {
    let path = if global {
        settings::home_config_path()
            .ok_or_else(|| anyhow::anyhow!("cannot determine HOME for ~/.cih/config.toml"))?
    } else {
        settings::repo_config_path(repo)
    };

    if path.exists() && !force {
        bail!(
            "{} already exists — pass --force to overwrite, or edit it directly",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, settings::starter_toml())?;
    println!("\n  Wrote starter settings to {}", path.display());
    println!("  All options are commented out (defaults unchanged). Uncomment to override.");
    println!("  Run `cih config show` to see effective values.\n");
    Ok(())
}

/// Interactive editor for decompile config. Reads from and writes to
/// `<repo>/cih.decompile.toml`.
pub fn run_config_decompile(repo: &Path) -> Result<()> {
    let theme = ColorfulTheme::default();
    let mut cfg = DecompileConfig::load_or_default(repo);

    println!("\n  CIH — Decompile Config");
    println!("  ─────────────────────────────────");
    println!("  File: {}\n", repo.join("cih.decompile.toml").display());

    // ── Tool selection ─────────────────────────────────────────────────────
    let tools = ["vineflower", "cfr", "jadx"];
    let tool_default = tools
        .iter()
        .position(|t| *t == cfg.tool.as_str())
        .unwrap_or(0);
    let tool_idx = Select::with_theme(&theme)
        .with_prompt("Decompiler tool")
        .items(&tools)
        .default(tool_default)
        .interact()?;
    cfg.tool = tools[tool_idx].to_string();

    // ── Tool path ──────────────────────────────────────────────────────────
    if cfg.tool == "vineflower" || cfg.tool == "cfr" {
        cfg.tool_jar = Some(
            Input::<String>::with_theme(&theme)
                .with_prompt("Path to vineflower.jar / cfr.jar")
                .default(cfg.tool_jar.clone().unwrap_or_default())
                .allow_empty(true)
                .interact_text()?,
        );
        if cfg.tool_jar.as_deref() == Some("") {
            cfg.tool_jar = None;
        }
    } else {
        cfg.tool_bin = Some(
            Input::<String>::with_theme(&theme)
                .with_prompt("Path to jadx binary")
                .default(cfg.tool_bin.clone().unwrap_or_default())
                .allow_empty(true)
                .interact_text()?,
        );
        if cfg.tool_bin.as_deref() == Some("") {
            cfg.tool_bin = None;
        }
    }

    // ── Cache directory ────────────────────────────────────────────────────
    let cache_dir: String = Input::with_theme(&theme)
        .with_prompt("Cache directory (repo-relative or absolute)")
        .default(
            cfg.cache_dir
                .clone()
                .unwrap_or_else(|| ".cih/decompiled".into()),
        )
        .interact_text()?;
    cfg.cache_dir = Some(cache_dir);

    // ── Sources loop ───────────────────────────────────────────────────────
    loop {
        println!();
        if cfg.sources.is_empty() {
            println!("  No JAR sources configured yet.");
        } else {
            println!("  Current sources:");
            for (i, s) in cfg.sources.iter().enumerate() {
                println!("    {}. dir={:?}  prefix={:?}", i + 1, s.dir, s.prefix);
            }
        }
        println!();

        let mut actions = vec!["Add source", "Done"];
        if !cfg.sources.is_empty() {
            actions.insert(1, "Remove source");
        }
        let choice = Select::with_theme(&theme)
            .with_prompt("Action")
            .items(&actions)
            .default(0)
            .interact()?;

        match actions[choice] {
            "Add source" => {
                let dir: String = Input::with_theme(&theme)
                    .with_prompt("JAR directory (repo-relative or absolute, ~ ok)")
                    .allow_empty(false)
                    .interact_text()?;
                let prefix: String = Input::with_theme(&theme)
                    .with_prompt("Filename prefix (e.g. mfa-, bank-auth-)")
                    .allow_empty(false)
                    .interact_text()?;
                cfg.sources.push(DecompileSource { dir, prefix });
            }
            "Remove source" => {
                let labels: Vec<String> = cfg
                    .sources
                    .iter()
                    .enumerate()
                    .map(|(i, s)| format!("{}. dir={:?}  prefix={:?}", i + 1, s.dir, s.prefix))
                    .collect();
                let idx = Select::with_theme(&theme)
                    .with_prompt("Remove which source?")
                    .items(&labels)
                    .default(0)
                    .interact()?;
                cfg.sources.remove(idx);
            }
            _ => break,
        }
    }

    // ── Save ───────────────────────────────────────────────────────────────
    cfg.save(repo)?;
    println!("\n  Saved to cih.decompile.toml ✓\n");
    Ok(())
}
