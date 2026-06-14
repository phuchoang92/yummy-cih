use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use cih_core::RepoMap;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::scan::OwnedJavaFile;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ScopeRequest {
    pub all: bool,
    pub modules: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_decompiled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScopeFile {
    pub repo_root: String,
    pub version: String,
    pub selection: ScopeRequest,
    pub modules: Vec<String>,
    pub file_count: u64,
    pub files: Vec<String>,
}

impl ScopeRequest {
    pub(crate) fn from_toml(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub(crate) fn has_selector(&self) -> bool {
        self.all || !self.modules.is_empty() || !self.include.is_empty()
    }
}

pub(crate) fn resolve(
    repo_map: &RepoMap,
    java_files: &[OwnedJavaFile],
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
    for file in java_files {
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

pub(crate) fn write_scope_file(scope_file: &ScopeFile) -> Result<PathBuf> {
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
    file: &OwnedJavaFile,
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

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{BuildSystem, ModuleInfo, SpringSignal};

    fn repo_map() -> RepoMap {
        RepoMap {
            root: "/repo".into(),
            build_system: BuildSystem::Maven,
            total_java_files: 3,
            total_loc: 30,
            modules: vec![
                module("app", "app"),
                module("infra", "infra"),
                module(".workspace-dependencies", ".workspace-dependencies"),
            ],
            jars: Vec::new(),
            decompiled_dirs: vec![".workspace-dependencies".into()],
        }
    }

    fn module(name: &str, rel_path: &str) -> ModuleInfo {
        ModuleInfo {
            name: name.into(),
            rel_path: rel_path.into(),
            build_file: None,
            java_files: 1,
            loc: 10,
            packages: Vec::new(),
            spring: SpringSignal::default(),
            depends_on: Vec::new(),
        }
    }

    fn java_files() -> Vec<OwnedJavaFile> {
        vec![
            owned("app/src/main/java/App.java", Some("app")),
            owned("infra/src/main/java/Repo.java", Some("infra")),
            owned(
                ".workspace-dependencies/lib/src/main/java/Lib.java",
                Some(".workspace-dependencies"),
            ),
        ]
    }

    fn owned(rel: &str, module_rel: Option<&str>) -> OwnedJavaFile {
        OwnedJavaFile {
            rel: rel.into(),
            module_rel: module_rel.map(str::to_string),
        }
    }

    #[test]
    fn module_selector_keeps_only_that_modules_files() {
        let scope = resolve(
            &repo_map(),
            &java_files(),
            ScopeRequest {
                modules: vec!["app".into()],
                ..ScopeRequest::default()
            },
        )
        .unwrap();

        assert_eq!(scope.file_count, 1);
        assert_eq!(scope.modules, vec!["app"]);
        assert_eq!(scope.files, vec!["app/src/main/java/App.java"]);
    }

    #[test]
    fn all_excludes_decompiled_unless_requested() {
        let without_decompiled = resolve(
            &repo_map(),
            &java_files(),
            ScopeRequest {
                all: true,
                ..ScopeRequest::default()
            },
        )
        .unwrap();
        assert_eq!(without_decompiled.file_count, 2);

        let with_decompiled = resolve(
            &repo_map(),
            &java_files(),
            ScopeRequest {
                all: true,
                include_decompiled: true,
                ..ScopeRequest::default()
            },
        )
        .unwrap();
        assert_eq!(with_decompiled.file_count, 3);
    }

    #[test]
    fn exclude_glob_removes_matches_after_selection() {
        let scope = resolve(
            &repo_map(),
            &java_files(),
            ScopeRequest {
                all: true,
                exclude: vec!["infra/**".into()],
                ..ScopeRequest::default()
            },
        )
        .unwrap();

        assert_eq!(scope.files, vec!["app/src/main/java/App.java"]);
    }

    #[test]
    fn same_inputs_produce_same_version() {
        let request = ScopeRequest {
            all: true,
            ..ScopeRequest::default()
        };
        let first = resolve(&repo_map(), &java_files(), request.clone()).unwrap();
        let second = resolve(&repo_map(), &java_files(), request).unwrap();
        assert_eq!(first.version, second.version);
    }

    #[test]
    fn scope_request_loads_from_toml() {
        let dir = std::env::temp_dir().join(format!(
            "cih-scope-toml-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cih.scope.toml");
        fs::write(
            &path,
            r#"
modules = ["app"]
exclude = ["**/generated/**"]
include_decompiled = true
"#,
        )
        .unwrap();

        let request = ScopeRequest::from_toml(&path).unwrap();
        assert_eq!(request.modules, vec!["app"]);
        assert_eq!(request.exclude, vec!["**/generated/**"]);
        assert!(request.include_decompiled);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parent_module_selects_descendant_files() {
        // `payments` is an aggregator (rel "payments") that owns no files directly;
        // selecting it by name must pull in both child modules' files.
        let repo_map = RepoMap {
            root: "/repo".into(),
            build_system: BuildSystem::Maven,
            total_java_files: 2,
            total_loc: 20,
            modules: vec![
                module("payments", "payments"),
                module("api", "payments/api"),
                module("core", "payments/core"),
            ],
            jars: Vec::new(),
            decompiled_dirs: Vec::new(),
        };
        let files = vec![
            owned("payments/api/src/main/java/Api.java", Some("payments/api")),
            owned(
                "payments/core/src/main/java/Core.java",
                Some("payments/core"),
            ),
        ];

        let scope = resolve(
            &repo_map,
            &files,
            ScopeRequest {
                modules: vec!["payments".into()],
                ..ScopeRequest::default()
            },
        )
        .unwrap();

        assert_eq!(scope.file_count, 2);
        assert_eq!(
            scope.files,
            vec![
                "payments/api/src/main/java/Api.java",
                "payments/core/src/main/java/Core.java",
            ]
        );
    }

    #[test]
    fn duplicate_module_name_selects_all_and_unknown_bails() {
        // Two distinct modules share the fallback basename "billing".
        let repo_map = RepoMap {
            root: "/repo".into(),
            build_system: BuildSystem::Gradle,
            total_java_files: 2,
            total_loc: 20,
            modules: vec![
                module("billing", "services/billing"),
                module("billing", "legacy/billing"),
            ],
            jars: Vec::new(),
            decompiled_dirs: Vec::new(),
        };
        let files = vec![
            owned("services/billing/src/A.java", Some("services/billing")),
            owned("legacy/billing/src/B.java", Some("legacy/billing")),
        ];

        let scope = resolve(
            &repo_map,
            &files,
            ScopeRequest {
                modules: vec!["billing".into()],
                ..ScopeRequest::default()
            },
        )
        .unwrap();
        assert_eq!(scope.file_count, 2);

        let unknown = resolve(
            &repo_map,
            &files,
            ScopeRequest {
                modules: vec!["nope".into()],
                ..ScopeRequest::default()
            },
        );
        assert!(unknown.is_err());
    }
}
