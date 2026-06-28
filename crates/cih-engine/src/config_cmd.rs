//! `cih config decompile` — interactive editor for `cih.decompile.toml`.

use std::path::Path;

use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Input, Select};

use crate::decompile_config::{DecompileConfig, DecompileSource};

/// Interactive editor for decompile config. Reads from and writes to
/// `<repo>/cih.decompile.toml`.
pub fn run_config_decompile(repo: &Path) -> Result<()> {
    let theme = ColorfulTheme::default();
    let mut cfg = DecompileConfig::load_or_default(repo);

    println!("\n  CIH — Decompile Config");
    println!("  ─────────────────────────────────");
    println!("  File: {}\n", repo.join("cih.decompile.toml").display());

    // ── Tool selection ─────────────────────────────────────────────────────
    let tools = ["cfr", "jadx"];
    let tool_default = tools.iter().position(|t| *t == cfg.tool.as_str()).unwrap_or(0);
    let tool_idx = Select::with_theme(&theme)
        .with_prompt("Decompiler tool")
        .items(&tools)
        .default(tool_default)
        .interact()?;
    cfg.tool = tools[tool_idx].to_string();

    // ── Tool path ──────────────────────────────────────────────────────────
    if cfg.tool == "cfr" {
        cfg.tool_jar = Some(
            Input::<String>::with_theme(&theme)
                .with_prompt("Path to cfr.jar")
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
