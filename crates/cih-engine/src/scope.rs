use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cih_core::RepoMap;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::scan::OwnedSourceFile;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScopeRequest {
    pub all: bool,
    pub modules: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_decompiled: bool,
    /// Language filter: only include files belonging to these languages.
    /// Empty = all registered languages (default behavior unchanged).
    pub languages: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeFile {
    pub repo_root: String,
    pub version: String,
    pub selection: ScopeRequest,
    pub modules: Vec<String>,
    pub file_count: u64,
    pub files: Vec<String>,
}

impl ScopeRequest {
    pub fn from_toml(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn has_selector(&self) -> bool {
        self.all || !self.modules.is_empty() || !self.include.is_empty()
    }
}

pub fn resolve(
    repo_map: &RepoMap,
    source_files: &[OwnedSourceFile],
    request: ScopeRequest,
) -> Result<ScopeFile> {
    if !request.has_selector() {
        bail!("choose a scope before resolving files");
    }

    let rels_by_name = rels_by_name(repo_map);
    let module_by_rel = module_name_by_rel(repo_map);
    let include_globs = build_globs(&request.include)?;
    let exclude_globs = build_globs(&request.exclude)?;
    let selected_module_rels = selected_module_rels(&request, &rels_by_name)?;

    let mut files = Vec::new();
    let mut touched_module_rels = BTreeSet::new();
    for file in source_files {
        if !matches_selector(
            file,
            &request,
            &selected_module_rels,
            include_globs.as_ref(),
        ) {
            continue;
        }
        if !request.include_decompiled && is_decompiled(&file.rel, &repo_map.decompiled_dirs) {
            continue;
        }
        if exclude_globs
            .as_ref()
            .is_some_and(|globs| globs.is_match(&file.rel))
        {
            continue;
        }
        if !matches_language_filter(&file.language, &request.languages) {
            continue;
        }
        files.push(file.rel.clone());
        if let Some(module_rel) = &file.module_rel {
            touched_module_rels.insert(module_rel.clone());
        }
    }
    files.sort();
    files.dedup();

    let modules = touched_module_rels
        .into_iter()
        .filter_map(|rel| module_by_rel.get(&rel).cloned())
        .collect::<Vec<_>>();
    let version = scope_version(&files, &request)?;

    Ok(ScopeFile {
        repo_root: repo_map.root.clone(),
        version,
        selection: request,
        modules,
        file_count: files.len() as u64,
        files,
    })
}

pub fn write_scope_file(scope_file: &ScopeFile) -> Result<PathBuf> {
    let cih_dir = Path::new(&scope_file.repo_root).join(".cih");
    fs::create_dir_all(&cih_dir)
        .with_context(|| format!("failed to create {}", cih_dir.display()))?;
    let output_path = cih_dir.join("scope.json");
    let encoded = serde_json::to_string_pretty(scope_file)?;
    fs::write(&output_path, encoded.as_bytes())
        .with_context(|| format!("failed to write {}", output_path.display()))?;
    Ok(output_path)
}

/// name -> every rel_path registered under it. A multimap (not a 1:1 map) so two
/// modules sharing a fallback basename are both selectable by that name.
fn rels_by_name(repo_map: &RepoMap) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for module in &repo_map.modules {
        map.entry(module.name.clone())
            .or_default()
            .push(module.rel_path.clone());
    }
    map
}

fn module_name_by_rel(repo_map: &RepoMap) -> BTreeMap<String, String> {
    repo_map
        .modules
        .iter()
        .map(|module| (module.rel_path.clone(), module.name.clone()))
        .collect()
}

fn selected_module_rels(
    request: &ScopeRequest,
    rels_by_name: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeSet<String>> {
    if request.all || request.modules.is_empty() {
        return Ok(BTreeSet::new());
    }

    let mut selected = BTreeSet::new();
    let mut missing = Vec::new();
    for name in &request.modules {
        match rels_by_name.get(name) {
            Some(rels) => selected.extend(rels.iter().cloned()),
            None => missing.push(name.clone()),
        }
    }
    if !missing.is_empty() {
        bail!("unknown module(s): {}", missing.join(", "));
    }
    Ok(selected)
}

/// A file's owner module matches a selected module when it IS that module or lives
/// in its subtree — so naming a Maven parent/aggregator pom (which owns no files
/// directly) pulls in its children's files instead of resolving to nothing.
fn module_matches(module_rel: Option<&str>, selected: &BTreeSet<String>) -> bool {
    module_rel.is_some_and(|m| {
        selected
            .iter()
            .any(|r| r == "." || m == r || m.starts_with(&format!("{r}/")))
    })
}

fn matches_selector(
    file: &OwnedSourceFile,
    request: &ScopeRequest,
    selected_module_rels: &BTreeSet<String>,
    include_globs: Option<&GlobSet>,
) -> bool {
    if request.all {
        return true;
    }
    if !request.modules.is_empty() {
        return module_matches(file.module_rel.as_deref(), selected_module_rels);
    }
    include_globs.is_some_and(|globs| globs.is_match(&file.rel))
}

fn build_globs(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder
            .add(Glob::new(pattern).with_context(|| format!("invalid glob pattern `{pattern}`"))?);
    }
    Ok(Some(builder.build()?))
}

/// Returns `true` when the file should be included given the languages filter.
/// Empty filter = all languages included.
fn matches_language_filter(file_language: &str, languages: &[String]) -> bool {
    if languages.is_empty() {
        return true;
    }
    languages.iter().any(|l| l == file_language)
}

fn is_decompiled(path: &str, decompiled_dirs: &[String]) -> bool {
    decompiled_dirs.iter().any(|dir| {
        let dir = dir.trim_end_matches('/');
        path == dir || path.starts_with(&format!("{dir}/"))
    })
}

fn scope_version(files: &[String], request: &ScopeRequest) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    for file in files {
        hasher.update(file.as_bytes());
        hasher.update(b"\n");
    }
    let selection = serde_json::to_vec(request)?;
    hasher.update(&selection);
    Ok(hasher.finalize().to_hex()[..16].to_string())
}
