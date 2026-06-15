use anyhow::{anyhow, Result};
use cih_core::{now_rfc3339, GroupEntry, GroupRegistry, Registry};

pub(crate) fn run_group_create(name: &str) -> Result<()> {
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

pub(crate) fn run_group_add(name: &str, repo: &str) -> Result<()> {
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

pub(crate) fn run_group_remove(name: &str, repo: &str) -> Result<()> {
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

pub(crate) fn run_group_list(json: bool) -> Result<()> {
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

pub(crate) fn run_group_sync(name: &str, json: bool) -> Result<()> {
    let summary = crate::group::sync_group(name)?;
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
