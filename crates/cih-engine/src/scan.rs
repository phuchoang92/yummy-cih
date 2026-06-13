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
use std::path::Path;

use anyhow::{Context, Result};
use cih_core::{BuildSystem, JarInfo, ModuleInfo, RepoMap, SpringSignal};

mod build_files;
mod ignore_rules;
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
use report::print_summary;
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

pub fn run_scan(repo: &Path, json: bool) -> Result<()> {
    let repo_map = scan_repo(repo)?;
    let cih_dir = Path::new(&repo_map.root).join(".cih");
    fs::create_dir_all(&cih_dir)
        .with_context(|| format!("failed to create {}", cih_dir.display()))?;
    let output_path = cih_dir.join("repo-map.json");
    let encoded = serde_json::to_string_pretty(&repo_map)?;
    fs::write(&output_path, encoded.as_bytes())
        .with_context(|| format!("failed to write {}", output_path.display()))?;

    if json {
        println!("{encoded}");
    } else {
        print_summary(&repo_map, &output_path);
    }
    Ok(())
}

fn scan_repo(repo: &Path) -> Result<RepoMap> {
    let root = repo
        .canonicalize()
        .with_context(|| format!("failed to resolve repo path {}", repo.display()))?;
    let files = walk_repository_paths(&root)?;
    let _total_scanned_bytes: u64 = files.iter().map(|file| file.size).sum();
    let java_files = collect_java_files(&root, &files);
    let decompiled_dirs = collect_decompiled_dirs(&files);
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

    let artifact_to_name: BTreeMap<String, String> = candidates
        .iter()
        .filter_map(|m| {
            m.artifact_key
                .as_ref()
                .map(|key| (key.clone(), m.name.clone()))
        })
        .collect();
    let mut aggregates: BTreeMap<String, ModuleAggregate> = BTreeMap::new();

    for java in &java_files {
        if let Some(module_rel) = find_owner_module(&candidates, &java.path) {
            let aggregate = aggregates.entry(module_rel.to_string()).or_default();
            aggregate.java_files += 1;
            aggregate.loc += java.loc;
            if let Some(package) = &java.package {
                aggregate.packages.insert(package.clone());
            }
            add_spring_signal(&mut aggregate.spring, &java.spring);
        }
    }

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
            }
        })
        .collect();
    modules.sort_by(|a, b| a.rel_path.cmp(&b.rel_path).then(a.name.cmp(&b.name)));

    Ok(RepoMap {
        root: normalize_path(root),
        build_system: detect_build_system(&modules),
        total_java_files: java_files.len() as u64,
        total_loc: java_files.iter().map(|f| f.loc).sum(),
        modules,
        jars: Vec::<JarInfo>::new(),
        decompiled_dirs,
    })
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

        let repo_map = scan_repo(&repo.path).unwrap();
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
    }
}
