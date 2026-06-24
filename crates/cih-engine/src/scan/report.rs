//! Human-readable scan summary: the module table + a deterministic
//! "what to index first" recommendation with a rough cost estimate.

use std::path::Path;

use cih_core::{ModuleInfo, RepoMap};

const PARSE_MS_PER_FILE: u64 = 25;
const EST_NODES_PER_FILE: u64 = 17;

#[doc(hidden)]
pub fn print_summary(repo_map: &RepoMap, output_path: &Path) {
    println!("Repo: {}", repo_map.root);
    println!("Build system: {:?}", repo_map.build_system);
    println!(
        "Java files: {} - LOC: {}",
        repo_map.total_java_files,
        format_count(repo_map.total_loc)
    );
    if !repo_map.decompiled_dirs.is_empty() {
        println!("Decompiler dirs: {}", repo_map.decompiled_dirs.join(", "));
    }
    println!("Repo map: {}", output_path.display());
    println!();
    println!(
        "{:<32} {:>7} {:>8} {:>5} {:>5} {:>5} {:>7} {:>8} {:>10}",
        "Module", ".java", "LOC", "svc", "ctrl", "repo", "entity", "mapping", "est.nodes"
    );
    for module in &repo_map.modules {
        let display = if module.rel_path == "." {
            module.name.clone()
        } else {
            format!("{} ({})", module.name, module.rel_path)
        };
        println!(
            "{:<32} {:>7} {:>8} {:>5} {:>5} {:>5} {:>7} {:>8} {:>10}",
            truncate(&display, 32),
            module.java_files,
            format_count(module.loc),
            module.spring.services,
            module.spring.controllers,
            module.spring.repositories,
            module.spring.entities,
            module.spring.mappings,
            format!("~{}", format_count(module.java_files * EST_NODES_PER_FILE))
        );
    }
    println!();
    println!("{}", recommendation(repo_map));
}

fn recommendation(repo_map: &RepoMap) -> String {
    let mut modules: Vec<&ModuleInfo> = repo_map
        .modules
        .iter()
        .filter(|m| m.java_files > 0 && !is_deferred_module(m))
        .collect();
    modules.sort_by(|a, b| {
        module_score(b)
            .cmp(&module_score(a))
            .then(a.rel_path.cmp(&b.rel_path))
            .then(a.name.cmp(&b.name))
    });

    if modules.is_empty() {
        return "Recommend: no application Java modules found to index.".into();
    }

    let selected: Vec<&ModuleInfo> = modules.into_iter().take(3).collect();
    let files: u64 = selected.iter().map(|m| m.java_files).sum();
    let estimated_ms = files * PARSE_MS_PER_FILE;
    let nodes = files * EST_NODES_PER_FILE;
    let names = selected
        .iter()
        .map(|m| m.name.as_str())
        .collect::<Vec<_>>()
        .join(" + ");
    format!(
        "Recommend: start with `{names}` (~{} nodes, ~{}); defer generated/decompiled/third-party paths. Or `--all` for the full repo.",
        format_count(nodes),
        format_duration_ms(estimated_ms)
    )
}

fn module_score(module: &ModuleInfo) -> u64 {
    let spring = module.spring.controllers
        + module.spring.services
        + module.spring.repositories
        + module.spring.components
        + module.spring.configs
        + module.spring.entities
        + module.spring.mappings;
    spring as u64 * 10_000 + module.java_files
}

fn is_deferred_module(module: &ModuleInfo) -> bool {
    let rel = module.rel_path.to_ascii_lowercase();
    rel.starts_with(".workspace-dependencies")
        || rel.contains("/generated")
        || rel.contains("/auto-generated")
        || rel.contains("/vendor")
        || rel.contains("/third_party")
        || rel.contains("/3rdparty")
}

fn format_duration_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{:.1}min", ms as f64 / 60_000.0)
    }
}

fn format_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

/// Char-safe truncation with an ellipsis (never slices a multibyte boundary).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let take = max.saturating_sub(3);
        let prefix: String = s.chars().take(take).collect();
        format!("{prefix}...")
    }
}
