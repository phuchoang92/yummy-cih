//! Phase 3 discovery scan: walk the repo, detect modules, and summarize each
//! module (file counts, LOC, packages, framework signals) WITHOUT tree-sitter.
//! This module file holds the shared data model + orchestration; the work is
//! split across `scan/` submodules:
//!   - `ignore_rules` - ignore lists + path/dir/extension predicates
//!   - `walk`         - gitignore-aware filesystem walk
//!   - `paths`        - relative-path helpers
//!   - `build_files`  - pom.xml / build.gradle / package.json / pyproject.toml parsing
//!   - `modules`      - module detection, ownership, build-system
//!   - `source_scan`  - per-file LOC / package / framework extraction (registry-driven)
//!   - `report`       - summary table + recommendation

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cih_core::{auto_detect_architecture, BuildSystem, JarInfo, ModuleInfo, RepoMap};

pub mod build_files;
pub mod ignore_rules;
pub mod jars;
mod modules;
mod paths;
mod report;
pub mod source_scan;
mod walk;

use modules::{
    detect_build_system, detect_modules, ensure_unassigned_source_module, find_owner_module,
    upsert_candidate,
};
use paths::normalize_path;
pub use report::print_summary;
use source_scan::{collect_decompiled_dirs, collect_source_files};
use walk::walk_repository_paths;

// --- shared data model (used across the scan submodules) ---

#[derive(Clone, Debug)]
pub struct ScannedFile {
    pub path: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct SourceFileInfo {
    pub path: String,
    pub language: String,
    pub loc: u64,
    pub package: Option<String>,
    pub frameworks: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct BuildMeta {
    pub group_id: String,
    pub artifact_id: String,
    pub deps: Vec<String>,
    pub modules: Vec<String>,
}

#[derive(Clone, Debug)]
struct ModuleCandidate {
    name: String,
    rel_path: String,
    build_file: Option<String>,
    build_system: BuildSystem,
    module_key: Option<String>,
    deps: Vec<String>,
}

#[derive(Default)]
struct ModuleAggregate {
    source_files: u64,
    source_loc: u64,
    packages: BTreeSet<String>,
    frameworks: BTreeSet<String>,
    per_language: BTreeMap<String, u64>,
}

#[derive(Clone, Debug)]
pub struct ScanResult {
    pub repo_map: RepoMap,
    pub source_files: Vec<OwnedSourceFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedSourceFile {
    pub rel: String,
    pub module_rel: Option<String>,
    pub language: String,
}

pub fn run_scan(repo: &Path, json: bool) -> Result<()> {
    let scan = scan_repo(repo)?;
    let output_path = write_repo_map(&scan.repo_map)?;
    let encoded = serde_json::to_string_pretty(&scan.repo_map)?;

    if json {
        println!("{encoded}");
    } else {
        print_summary(&scan.repo_map, &output_path);
    }
    Ok(())
}

pub fn write_repo_map(repo_map: &RepoMap) -> Result<PathBuf> {
    let cih_dir = Path::new(&repo_map.root).join(".cih");
    fs::create_dir_all(&cih_dir)
        .with_context(|| format!("failed to create {}", cih_dir.display()))?;
    let output_path = cih_dir.join("repo-map.json");
    let encoded = serde_json::to_string_pretty(&repo_map)?;
    fs::write(&output_path, encoded.as_bytes())
        .with_context(|| format!("failed to write {}", output_path.display()))?;
    Ok(output_path)
}

/// Shared registry builder — used by both scan and analyze.
pub fn default_scan_registry() -> cih_parse::LanguageRegistry {
    let mut r = cih_parse::LanguageRegistry::new();
    r.register(cih_lang::java::JavaProvider::new());
    r.register(cih_lang::typescript::TypescriptProvider::new());
    r.register(cih_lang::python::PythonProvider::new());
    r.register(cih_lang::kotlin::KotlinProvider::new());
    r.register(cih_lang::go::GoProvider::new());
    r.register(cih_lang::rust_lang::RustProvider::new());
    r.register(cih_lang::csharp::CSharpProvider::new());
    r.register(cih_lang::ruby::RubyProvider::new());
    r.register(cih_lang::php::PhpProvider::new());
    r.register(cih_lang::scala::ScalaProvider::new());
    r.register(cih_lang::cpp::CppProvider::new());
    r.register(cih_lang::bash::BashProvider::new());
    r.register(cih_lang::elixir::ElixirProvider::new());
    r
}

pub fn scan_repo(repo: &Path) -> Result<ScanResult> {
    let root = repo
        .canonicalize()
        .with_context(|| format!("failed to resolve repo path {}", repo.display()))?;

    let span = tracing::info_span!("scan", repo = %root.display());
    let _enter = span.enter();

    tracing::info!(repo = %root.display(), "starting repository walk");

    let files = walk_repository_paths(&root)?;
    let total_bytes: u64 = files.iter().map(|file| file.size).sum();
    tracing::info!(
        total_files = files.len(),
        total_bytes,
        "filesystem walk complete"
    );

    let registry = default_scan_registry();
    let source_files_info = collect_source_files(&root, &files, &registry);
    let decompiled_dirs = collect_decompiled_dirs(&files);
    tracing::info!(
        source_files = source_files_info.len(),
        decompiled_dirs = decompiled_dirs.len(),
        "source files collected"
    );

    let mut candidates = detect_modules(&root, &files)?;

    for decompiled in &decompiled_dirs {
        upsert_candidate(
            &mut candidates,
            ModuleCandidate {
                name: decompiled.clone(),
                rel_path: decompiled.clone(),
                build_file: None,
                build_system: BuildSystem::None,
                module_key: None,
                deps: Vec::new(),
            },
        );
    }

    if candidates.is_empty() {
        let root_name = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("root")
            .to_string();
        candidates.push(ModuleCandidate {
            name: root_name,
            rel_path: ".".into(),
            build_file: None,
            build_system: BuildSystem::None,
            module_key: None,
            deps: Vec::new(),
        });
    }
    ensure_unassigned_source_module(&mut candidates, &source_files_info, &root);

    tracing::info!(modules = candidates.len(), "modules detected");
    for c in &candidates {
        tracing::debug!(module = %c.name, path = %c.rel_path, build = ?c.build_system, "module");
    }

    let (all_deps, own_group_prefix, key_to_name) = jar_discovery_inputs(&candidates);
    let (aggregates, mut owned_source_files) =
        collect_source_aggregates(&candidates, &source_files_info);
    let modules = build_modules_from_aggregates(candidates.clone(), aggregates, &key_to_name);

    owned_source_files.sort_by(|a, b| a.rel.cmp(&b.rel));

    let discovered_jars = discover_and_link_jars(&root, &all_deps, &own_group_prefix);
    tracing::info!(jars = discovered_jars.len(), "JAR discovery complete");

    let total_source_loc: u64 = source_files_info.iter().map(|f| f.loc).sum();
    let total_source_files = source_files_info.len() as u64;

    let mut per_language: BTreeMap<String, u64> = BTreeMap::new();
    for sf in &source_files_info {
        *per_language.entry(sf.language.clone()).or_default() += 1;
    }

    let has_node = files.iter().any(|f| {
        f.path == "package.json"
            || f.path.ends_with("/package.json")
            || f.path == "nest-cli.json"
            || f.path.ends_with("/nest-cli.json")
    });
    let has_python = files.iter().any(|f| {
        f.path == "pyproject.toml"
            || f.path.ends_with("/pyproject.toml")
            || f.path == "setup.py"
            || f.path.ends_with("/setup.py")
            || f.path == "requirements.txt"
            || f.path.ends_with("/requirements.txt")
    });
    let java_build_system = detect_build_system(&modules);
    let has_java_build = matches!(java_build_system, BuildSystem::Maven | BuildSystem::Gradle);
    let build_system = if has_java_build && (has_node || has_python) {
        BuildSystem::Mixed
    } else if has_java_build {
        java_build_system
    } else if has_node {
        BuildSystem::Node
    } else if has_python {
        BuildSystem::Python
    } else {
        java_build_system
    };

    let mut repo_map = RepoMap {
        root: normalize_path(root),
        build_system,
        total_source_loc,
        total_source_files,
        per_language,
        modules,
        jars: discovered_jars,
        decompiled_dirs,
        architecture_hint: cih_core::ArchitectureHint::Unknown,
    };
    repo_map.architecture_hint = auto_detect_architecture(&repo_map);

    tracing::info!(
        source_files = total_source_files,
        total_source_loc,
        modules = repo_map.modules.len(),
        jars = repo_map.jars.len(),
        "scan complete"
    );

    Ok(ScanResult {
        repo_map,
        source_files: owned_source_files,
    })
}

fn jar_discovery_inputs(
    candidates: &[ModuleCandidate],
) -> (Vec<String>, String, BTreeMap<String, String>) {
    let all_deps: Vec<String> = {
        let mut set: BTreeSet<String> = BTreeSet::new();
        for candidate in candidates {
            set.extend(candidate.deps.iter().cloned());
        }
        set.into_iter().collect()
    };
    let own_group_prefix = candidates
        .iter()
        .find(|candidate| candidate.rel_path == ".")
        .or_else(|| candidates.first())
        .and_then(|candidate| candidate.module_key.as_ref())
        .and_then(|key| key.split(':').next())
        .unwrap_or("")
        .to_string();
    let key_to_name = candidates
        .iter()
        .filter_map(|candidate| {
            candidate
                .module_key
                .as_ref()
                .map(|key| (key.clone(), candidate.name.clone()))
        })
        .collect();
    (all_deps, own_group_prefix, key_to_name)
}

fn collect_source_aggregates(
    candidates: &[ModuleCandidate],
    source_files: &[SourceFileInfo],
) -> (BTreeMap<String, ModuleAggregate>, Vec<OwnedSourceFile>) {
    let mut aggregates: BTreeMap<String, ModuleAggregate> = BTreeMap::new();
    let mut owned_source_files = Vec::new();

    for sf in source_files {
        let module_rel = find_owner_module(candidates, &sf.path).map(str::to_string);
        owned_source_files.push(OwnedSourceFile {
            rel: sf.path.clone(),
            module_rel: module_rel.clone(),
            language: sf.language.clone(),
        });

        if let Some(module_rel) = module_rel {
            let aggregate = aggregates.entry(module_rel.to_string()).or_default();
            aggregate.source_files += 1;
            aggregate.source_loc += sf.loc;
            if let Some(package) = &sf.package {
                aggregate.packages.insert(package.clone());
            }
            aggregate.frameworks.extend(sf.frameworks.iter().cloned());
            *aggregate
                .per_language
                .entry(sf.language.clone())
                .or_default() += 1;
        }
    }

    (aggregates, owned_source_files)
}

fn build_modules_from_aggregates(
    candidates: Vec<ModuleCandidate>,
    mut aggregates: BTreeMap<String, ModuleAggregate>,
    key_to_name: &BTreeMap<String, String>,
) -> Vec<ModuleInfo> {
    let mut modules: Vec<ModuleInfo> = candidates
        .into_iter()
        .map(|candidate| {
            let aggregate = aggregates.remove(&candidate.rel_path).unwrap_or_default();
            let mut depends_on: Vec<String> = candidate
                .deps
                .iter()
                .filter_map(|dep| key_to_name.get(dep).cloned())
                .filter(|name| name != &candidate.name)
                .collect();
            depends_on.sort();
            depends_on.dedup();

            let mut frameworks: Vec<String> = aggregate.frameworks.into_iter().collect();
            frameworks.sort();
            frameworks.dedup();

            ModuleInfo {
                name: candidate.name,
                rel_path: candidate.rel_path,
                build_file: candidate.build_file,
                source_files: aggregate.source_files,
                source_loc: aggregate.source_loc,
                packages: aggregate.packages.into_iter().collect(),
                depends_on,
                frameworks,
                per_language: aggregate.per_language,
            }
        })
        .collect();
    modules.sort_by(|a, b| a.rel_path.cmp(&b.rel_path).then(a.name.cmp(&b.name)));
    modules
}

fn discover_and_link_jars(
    root: &Path,
    all_deps: &[String],
    own_group_prefix: &str,
) -> Vec<JarInfo> {
    jars::discover_jars(root, all_deps, own_group_prefix)
}
