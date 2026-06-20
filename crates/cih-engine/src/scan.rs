//! Phase 3 discovery scan: walk the repo, detect Maven/Gradle modules, and
//! summarize each module (file counts, LOC, packages, Spring signal) WITHOUT
//! tree-sitter. This module file holds the shared data model + orchestration;
//! the work is split across `scan/` submodules:
//!   - `ignore_rules` - ignore lists + path/dir/extension predicates
//!   - `walk`         - gitignore-aware filesystem walk
//!   - `paths`        - relative-path helpers
//!   - `build_files`  - pom.xml / build.gradle parsing
//!   - `modules`      - module detection, ownership, build-system
//!   - `java_scan`    - per-file LOC / package / Spring-signal extraction
//!   - `report`       - summary table + recommendation

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cih_core::{auto_detect_architecture, BuildSystem, JarInfo, ModuleInfo, RepoMap, SpringSignal};

mod build_files;
mod ignore_rules;
mod jars;
mod java_scan;
mod modules;
mod paths;
mod report;
mod walk;

use java_scan::{add_spring_signal, collect_decompiled_dirs, collect_java_files};
use modules::{
    detect_build_system, detect_modules, ensure_unassigned_java_module, find_owner_module,
    upsert_candidate,
};
use paths::normalize_path;
pub(crate) use report::print_summary;
use walk::walk_repository_paths;

// --- shared data model (used across the scan submodules) ---

#[derive(Clone, Debug)]
struct ScannedFile {
    path: String,
    size: u64,
}

#[derive(Clone, Debug)]
struct JavaFileInfo {
    path: String,
    loc: u64,
    package: Option<String>,
    spring: SpringSignal,
}

#[derive(Clone, Debug)]
struct BuildMeta {
    group_id: String,
    artifact_id: String,
    deps: Vec<String>,
    modules: Vec<String>,
}

#[derive(Clone, Debug)]
struct ModuleCandidate {
    name: String,
    rel_path: String,
    build_file: Option<String>,
    build_system: BuildSystem,
    artifact_key: Option<String>,
    deps: Vec<String>,
}

#[derive(Default)]
struct ModuleAggregate {
    java_files: u64,
    loc: u64,
    packages: BTreeSet<String>,
    spring: SpringSignal,
}

#[derive(Clone, Debug)]
pub(crate) struct ScanResult {
    pub repo_map: RepoMap,
    pub source_files: Vec<OwnedSourceFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnedSourceFile {
    pub rel: String,
    pub module_rel: Option<String>,
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

pub(crate) fn write_repo_map(repo_map: &RepoMap) -> Result<PathBuf> {
    let cih_dir = Path::new(&repo_map.root).join(".cih");
    fs::create_dir_all(&cih_dir)
        .with_context(|| format!("failed to create {}", cih_dir.display()))?;
    let output_path = cih_dir.join("repo-map.json");
    let encoded = serde_json::to_string_pretty(&repo_map)?;
    fs::write(&output_path, encoded.as_bytes())
        .with_context(|| format!("failed to write {}", output_path.display()))?;
    Ok(output_path)
}

pub(crate) fn scan_repo(repo: &Path) -> Result<ScanResult> {
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

    let java_files = collect_java_files(&root, &files);
    let decompiled_dirs = collect_decompiled_dirs(&files);
    tracing::info!(
        java_files = java_files.len(),
        decompiled_dirs = decompiled_dirs.len(),
        "Java files collected"
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
                artifact_key: None,
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
            artifact_key: None,
            deps: Vec::new(),
        });
    }
    ensure_unassigned_java_module(&mut candidates, &java_files, &root);

    tracing::info!(modules = candidates.len(), "modules detected");
    for c in &candidates {
        tracing::debug!(module = %c.name, path = %c.rel_path, build = ?c.build_system, "module");
    }

    let (all_deps, own_group_prefix, artifact_to_name) = jar_discovery_inputs(&candidates);
    let (aggregates, mut owned_source_files) = collect_java_aggregates(&candidates, &java_files);
    let modules = build_modules_from_aggregates(candidates.clone(), aggregates, &artifact_to_name);

    let extra_ts = collect_extra_source_files(&candidates, &files, &[".ts", ".tsx"]);
    let extra_py = collect_extra_source_files(&candidates, &files, &[".py"]);
    let ts_count = extra_ts.len() as u64;
    let py_count = extra_py.len() as u64;
    let java_count = java_files.len() as u64;
    owned_source_files.extend(extra_ts);
    owned_source_files.extend(extra_py);
    owned_source_files.sort_by(|a, b| a.rel.cmp(&b.rel));

    let discovered_jars = discover_and_link_jars(&root, &all_deps, &own_group_prefix);
    tracing::info!(jars = discovered_jars.len(), "JAR discovery complete");

    let total_loc: u64 = java_files.iter().map(|f| f.loc).sum();
    let total_source_files = java_count + ts_count + py_count;

    let mut per_language: BTreeMap<String, u64> = BTreeMap::new();
    if java_count > 0 {
        per_language.insert("java".into(), java_count);
    }
    if ts_count > 0 {
        per_language.insert("typescript".into(), ts_count);
    }
    if py_count > 0 {
        per_language.insert("python".into(), py_count);
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

    let modules = annotate_frameworks(modules, &files);

    let mut repo_map = RepoMap {
        root: normalize_path(root),
        build_system,
        total_java_files: java_count,
        total_loc,
        total_source_files,
        per_language,
        modules,
        jars: discovered_jars,
        decompiled_dirs,
        architecture_hint: cih_core::ArchitectureHint::Unknown,
    };
    repo_map.architecture_hint = auto_detect_architecture(&repo_map);

    tracing::info!(
        java_files = java_files.len(),
        total_loc,
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
        .and_then(|candidate| candidate.artifact_key.as_ref())
        .and_then(|key| key.split(':').next())
        .unwrap_or("")
        .to_string();
    let artifact_to_name = candidates
        .iter()
        .filter_map(|candidate| {
            candidate
                .artifact_key
                .as_ref()
                .map(|key| (key.clone(), candidate.name.clone()))
        })
        .collect();
    (all_deps, own_group_prefix, artifact_to_name)
}

fn collect_java_aggregates(
    candidates: &[ModuleCandidate],
    java_files: &[JavaFileInfo],
) -> (BTreeMap<String, ModuleAggregate>, Vec<OwnedSourceFile>) {
    let mut aggregates: BTreeMap<String, ModuleAggregate> = BTreeMap::new();
    let mut owned_source_files = Vec::new();

    for java in java_files {
        let module_rel = find_owner_module(candidates, &java.path).map(str::to_string);
        owned_source_files.push(OwnedSourceFile {
            rel: java.path.clone(),
            module_rel: module_rel.clone(),
        });

        if let Some(module_rel) = module_rel {
            let aggregate = aggregates.entry(module_rel.to_string()).or_default();
            aggregate.java_files += 1;
            aggregate.loc += java.loc;
            if let Some(package) = &java.package {
                aggregate.packages.insert(package.clone());
            }
            add_spring_signal(&mut aggregate.spring, &java.spring);
        }
    }

    (aggregates, owned_source_files)
}

fn build_modules_from_aggregates(
    candidates: Vec<ModuleCandidate>,
    mut aggregates: BTreeMap<String, ModuleAggregate>,
    artifact_to_name: &BTreeMap<String, String>,
) -> Vec<ModuleInfo> {
    let mut modules: Vec<ModuleInfo> = candidates
        .into_iter()
        .map(|candidate| {
            let aggregate = aggregates.remove(&candidate.rel_path).unwrap_or_default();
            let mut depends_on: Vec<String> = candidate
                .deps
                .iter()
                .filter_map(|dep| artifact_to_name.get(dep).cloned())
                .filter(|name| name != &candidate.name)
                .collect();
            depends_on.sort();
            depends_on.dedup();

            ModuleInfo {
                name: candidate.name,
                rel_path: candidate.rel_path,
                build_file: candidate.build_file,
                java_files: aggregate.java_files,
                loc: aggregate.loc,
                packages: aggregate.packages.into_iter().collect(),
                spring: aggregate.spring,
                depends_on,
                frameworks: Vec::new(),
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

fn collect_extra_source_files(
    candidates: &[ModuleCandidate],
    files: &[ScannedFile],
    extensions: &[&str],
) -> Vec<OwnedSourceFile> {
    files
        .iter()
        .filter(|f| extensions.iter().any(|ext| f.path.ends_with(ext)))
        .map(|f| OwnedSourceFile {
            rel: f.path.clone(),
            module_rel: find_owner_module(candidates, &f.path).map(str::to_string),
        })
        .collect()
}

/// Populate `ModuleInfo.frameworks` based on spring signal and indicator files.
fn annotate_frameworks(mut modules: Vec<ModuleInfo>, files: &[ScannedFile]) -> Vec<ModuleInfo> {
    for module in &mut modules {
        let s = &module.spring;
        let has_spring = s.controllers > 0
            || s.services > 0
            || s.repositories > 0
            || s.components > 0
            || s.configs > 0
            || s.entities > 0
            || s.mappings > 0;
        if has_spring {
            module.frameworks.push("spring".into());
        }

        let module_prefix = if module.rel_path == "." {
            String::new()
        } else {
            format!("{}/", module.rel_path)
        };

        let has_node = files.iter().any(|f| {
            let name = f.path.trim_start_matches(&module_prefix as &str);
            (name == "package.json" || name == "nest-cli.json" || name == "tsconfig.json")
                && !name.contains('/')
        });
        if has_node {
            let framework = if files.iter().any(|f| {
                f.path.starts_with(&module_prefix) && f.path.ends_with("/nest-cli.json")
                    || f.path.trim_start_matches(&module_prefix as &str) == "nest-cli.json"
            }) {
                "nestjs"
            } else {
                "node"
            };
            module.frameworks.push(framework.into());
        }

        let has_python = files.iter().any(|f| {
            let name = f.path.trim_start_matches(&module_prefix as &str);
            (name == "pyproject.toml"
                || name == "setup.py"
                || name == "requirements.txt"
                || name == "setup.cfg")
                && !name.contains('/')
        });
        if has_python {
            module.frameworks.push("python".into());
        }
    }
    modules
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestRepo {
        path: PathBuf,
    }

    impl TestRepo {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("cih-engine-test-{unique}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.path.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn scan_repo_builds_modules_counts_and_sibling_deps() {
        let repo = TestRepo::new();
        repo.write(
            "pom.xml",
            r#"
              <project>
                <groupId>com.acme</groupId>
                <artifactId>root</artifactId>
                <modules><module>app</module><module>infra</module></modules>
              </project>
            "#,
        );
        repo.write(
            "app/pom.xml",
            r#"
              <project>
                <groupId>com.acme</groupId>
                <artifactId>app</artifactId>
                <dependencies>
                  <dependency><groupId>com.acme</groupId><artifactId>infra</artifactId></dependency>
                </dependencies>
              </project>
            "#,
        );
        repo.write(
            "app/src/main/java/com/acme/owner/OwnerController.java",
            "package com.acme.owner;\n@RestController\n@GetMapping(\"/owners\")\nclass OwnerController {}\n",
        );
        repo.write(
            "infra/build.gradle",
            "group = 'com.acme'\ndependencies { implementation('org.springframework:spring-core:6.0.0') }\n",
        );
        repo.write(
            "infra/src/main/java/com/acme/owner/OwnerRepository.java",
            "package com.acme.owner;\n@Repository\nclass OwnerRepository {}\n",
        );
        repo.write(
            "target/generated/Generated.java",
            "package ignored;\n@Service\nclass Generated {}\n",
        );
        repo.write(
            ".workspace-dependencies/lib/src/main/java/com/acme/lib/LibService.java",
            "package com.acme.lib;\n@Service\nclass LibService {}\n",
        );

        let scan = scan_repo(&repo.path).unwrap();
        let repo_map = &scan.repo_map;
        assert_eq!(repo_map.build_system, BuildSystem::Maven);
        assert_eq!(repo_map.total_java_files, 3);
        assert_eq!(repo_map.decompiled_dirs, vec![".workspace-dependencies"]);

        let app = repo_map.modules.iter().find(|m| m.name == "app").unwrap();
        assert_eq!(app.java_files, 1);
        assert_eq!(app.spring.controllers, 1);
        assert_eq!(app.spring.mappings, 1);
        assert_eq!(app.packages, vec!["com.acme.owner"]);
        assert_eq!(app.depends_on, vec!["infra"]);

        let infra = repo_map.modules.iter().find(|m| m.name == "infra").unwrap();
        assert_eq!(infra.java_files, 1);
        assert_eq!(infra.spring.repositories, 1);

        let decompiled = repo_map
            .modules
            .iter()
            .find(|m| m.name == ".workspace-dependencies")
            .unwrap();
        assert_eq!(decompiled.java_files, 1);
        assert_eq!(decompiled.spring.services, 1);

        assert_eq!(scan.source_files.len(), 3);
        assert_eq!(
            scan.source_files
                .iter()
                .find(|file| file.rel.ends_with("OwnerController.java"))
                .and_then(|file| file.module_rel.as_deref()),
            Some("app")
        );
        assert_eq!(
            scan.source_files
                .iter()
                .find(|file| file.rel.ends_with("OwnerRepository.java"))
                .and_then(|file| file.module_rel.as_deref()),
            Some("infra")
        );
        assert_eq!(
            scan.source_files
                .iter()
                .find(|file| file.rel.ends_with("LibService.java"))
                .and_then(|file| file.module_rel.as_deref()),
            Some(".workspace-dependencies")
        );
    }
}
