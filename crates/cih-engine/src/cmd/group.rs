use anyhow::{anyhow, bail, Result};
use cih_core::{now_rfc3339, GroupEntry, GroupRegistry, Registry};

fn validate_group_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("group name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        bail!(
            "invalid group name '{}': only alphanumeric, '-', and '_' are allowed",
            name
        );
    }
    Ok(())
}

pub fn run_group_create(name: &str) -> Result<()> {
    validate_group_name(name)?;
    let mut registry = GroupRegistry::load();
    if registry.find(name).is_some() {
        println!("Group '{name}' already exists.");
        return Ok(());
    }
    registry.upsert(GroupEntry {
        name: name.to_string(),
        repos: Vec::new(),
        created_at: now_rfc3339(),
    });
    registry.save()?;
    println!("Created group '{name}'.");
    Ok(())
}

pub fn run_group_add(name: &str, repo: &str) -> Result<()> {
    let repo_registry = Registry::load();
    let repo_name = repo_registry
        .find(repo)
        .map(|entry| entry.name.clone())
        .ok_or_else(|| anyhow!("repo '{repo}' is not registered; run analyze first"))?;

    let mut group_registry = GroupRegistry::load();
    let group = group_registry.find_mut(name).ok_or_else(|| {
        anyhow!("group '{name}' does not exist; run `cih-engine group create {name}` first")
    })?;
    if !group.repos.contains(&repo_name) {
        group.repos.push(repo_name.clone());
        group.repos.sort();
    }
    group_registry.save()?;
    println!("Added repo '{repo_name}' to group '{name}'.");
    Ok(())
}

pub fn run_group_remove(name: &str, repo: &str) -> Result<()> {
    let repo_registry = Registry::load();
    let repo_name = repo_registry
        .find(repo)
        .map(|entry| entry.name.clone())
        .unwrap_or_else(|| repo.to_string());

    let mut group_registry = GroupRegistry::load();
    let group = group_registry
        .find_mut(name)
        .ok_or_else(|| anyhow!("group '{name}' does not exist"))?;
    group.repos.retain(|item| item != &repo_name);
    group_registry.save()?;
    println!("Removed repo '{repo_name}' from group '{name}'.");
    Ok(())
}

pub fn run_group_list(json: bool) -> Result<()> {
    let registry = GroupRegistry::load();
    if json {
        println!("{}", serde_json::to_string_pretty(&registry)?);
        return Ok(());
    }

    if registry.groups.is_empty() {
        println!("No groups. Run `cih-engine group create <name>` first.");
        return Ok(());
    }

    println!("{:<24} {:>5}  repos", "name", "count");
    println!("{}", "-".repeat(80));
    for group in registry.groups {
        println!(
            "{:<24} {:>5}  {}",
            group.name,
            group.repos.len(),
            group.repos.join(", ")
        );
    }
    Ok(())
}

pub fn run_group_sync(name: &str, json: bool) -> Result<()> {
    validate_group_name(name)?;
    let summary = super::group_sync::sync_group(name)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!(
            "Synced group '{}' across {} repos: {} contract matches -> {}",
            summary.group, summary.repo_count, summary.contract_count, summary.output_path
        );
    }
    Ok(())
}

pub fn run_group_status(name: &str, json: bool) -> Result<()> {
    validate_group_name(name)?;
    let group_registry = GroupRegistry::load();
    let group = group_registry
        .find(name)
        .ok_or_else(|| anyhow!("group '{name}' does not exist"))?;
    let registry = Registry::load();
    let dir =
        cih_core::group_dir(name).ok_or_else(|| anyhow!("cannot determine HOME for group path"))?;
    let state = cih_core::SyncState::load(&dir);
    let contracts_exist = dir.join("contracts.jsonl").exists();
    let stale = cih_core::group_contracts_stale(group, &registry, state.as_ref(), contracts_exist);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "group": group.name,
                "repos": group.repos,
                "contracts_exist": contracts_exist,
                "contracts_synced_at": state.as_ref().map(|s| s.synced_at.clone()),
                "generation": state.as_ref().map(|s| s.generation),
                "stale": stale,
            }))?
        );
        return Ok(());
    }

    println!("group:      {}", group.name);
    println!("repos:      {}", group.repos.join(", "));
    match &state {
        Some(state) => println!(
            "last sync:  {} (generation {})",
            state.synced_at, state.generation
        ),
        None if contracts_exist => println!("last sync:  unknown (contracts exist but unstamped)"),
        None => println!("last sync:  never"),
    }
    println!(
        "contracts:  {}",
        if stale {
            "STALE — run `cih-engine group sync`"
        } else if contracts_exist {
            "fresh"
        } else {
            "not synced yet"
        }
    );
    Ok(())
}
