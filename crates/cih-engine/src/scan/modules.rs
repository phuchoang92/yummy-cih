//! Module detection (Maven/Gradle) to `ModuleCandidate`s, plus file-to-module
//! ownership (longest-prefix), and build-system detection. Builds a module TREE
//! on top of GitNexus's flat pom/gradle parsing.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use cih_core::{BuildSystem, ModuleInfo};

use super::build_files::{
    parse_gradle, parse_gradle_includes, parse_package_json, parse_pom, parse_pyproject_toml,
    parse_setup_cfg,
};
use super::paths::{join_rel, parent_rel, path_from_rel};
use super::{ModuleCandidate, ScannedFile, SourceFileInfo};

pub(super) fn detect_modules(root: &Path, files: &[ScannedFile]) -> Result<Vec<ModuleCandidate>> {
    tracing::debug!(
        total_files = files.len(),
        "modules: scanning for build files"
    );
    let mut modules = Vec::new();

    for file in files {
        let file_name = file.path.rsplit('/').next().unwrap_or_default();
        match file_name {
            "pom.xml" => {
                tracing::debug!(path = %file.path, "modules: found pom.xml");
                let content = fs::read_to_string(root.join(&file.path))
                    .with_context(|| format!("failed to read {}", file.path))?;
                let meta = parse_pom(&content);
                let rel_path = parent_rel(&file.path);
                let name = meta
                    .as_ref()
                    .map(|m| m.artifact_id.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| fallback_module_name(root, &rel_path));
                let module_key = meta
                    .as_ref()
                    .map(|m| format!("{}:{}", m.group_id, m.artifact_id));
                let deps = meta.as_ref().map(|m| m.deps.clone()).unwrap_or_default();
                let child_modules = meta.as_ref().map(|m| m.modules.clone()).unwrap_or_default();

                tracing::debug!(module = %name, path = %rel_path, deps = deps.len(), children = child_modules.len(), "modules: Maven module detected");
                upsert_candidate(
                    &mut modules,
                    ModuleCandidate {
                        name,
                        rel_path: rel_path.clone(),
                        build_file: Some(file.path.clone()),
                        build_system: BuildSystem::Maven,
                        module_key,
                        deps,
                    },
                );

                for child in child_modules {
                    let child_rel = join_rel(&rel_path, &child);
                    let child_name = fallback_module_name(root, &child_rel);
                    tracing::debug!(module = %child_name, path = %child_rel, "modules: Maven child module");
                    upsert_candidate(
                        &mut modules,
                        ModuleCandidate {
                            name: child_name,
                            rel_path: child_rel,
                            build_file: None,
                            build_system: BuildSystem::Maven,
                            module_key: None,
                            deps: Vec::new(),
                        },
                    );
                }
            }
            "build.gradle" | "build.gradle.kts" => {
                tracing::debug!(path = %file.path, "modules: found build.gradle");
                let content = fs::read_to_string(root.join(&file.path))
                    .with_context(|| format!("failed to read {}", file.path))?;
                let rel_path = parent_rel(&file.path);
                let meta = parse_gradle(&content, &root.join(path_from_rel(&rel_path)));
                let name = meta
                    .as_ref()
                    .map(|m| m.artifact_id.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| fallback_module_name(root, &rel_path));
                let module_key = meta
                    .as_ref()
                    .filter(|m| !m.group_id.is_empty())
                    .map(|m| format!("{}:{}", m.group_id, m.artifact_id));
                let deps = meta.as_ref().map(|m| m.deps.clone()).unwrap_or_default();

                tracing::debug!(module = %name, path = %rel_path, deps = deps.len(), "modules: Gradle module detected");
                upsert_candidate(
                    &mut modules,
                    ModuleCandidate {
                        name,
                        rel_path,
                        build_file: Some(file.path.clone()),
                        build_system: BuildSystem::Gradle,
                        module_key,
                        deps,
                    },
                );
            }
            "settings.gradle" | "settings.gradle.kts" => {
                tracing::debug!(path = %file.path, "modules: found settings.gradle");
                let content = fs::read_to_string(root.join(&file.path))
                    .with_context(|| format!("failed to read {}", file.path))?;
                let base_rel = parent_rel(&file.path);
                let includes = parse_gradle_includes(&content);
                tracing::debug!(
                    includes = includes.len(),
                    "modules: Gradle settings includes"
                );
                for include in includes {
                    let child_rel = join_rel(&base_rel, &include.replace(':', "/"));
                    let child_name = fallback_module_name(root, &child_rel);
                    tracing::debug!(module = %child_name, path = %child_rel, "modules: Gradle included subproject");
                    upsert_candidate(
                        &mut modules,
                        ModuleCandidate {
                            name: child_name,
                            rel_path: child_rel,
                            build_file: None,
                            build_system: BuildSystem::Gradle,
                            module_key: None,
                            deps: Vec::new(),
                        },
                    );
                }
            }
            "package.json" => {
                tracing::debug!(path = %file.path, "modules: found package.json");
                let content = fs::read_to_string(root.join(&file.path))
                    .with_context(|| format!("failed to read {}", file.path))?;
                let rel_path = parent_rel(&file.path);
                let meta = parse_package_json(&content);
                let manifest_name = meta.as_ref().map(|m| {
                    if m.group_id.is_empty() {
                        m.artifact_id.clone()
                    } else {
                        format!("{}/{}", m.group_id, m.artifact_id)
                    }
                });
                let name = manifest_name
                    .clone()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| fallback_module_name(root, &rel_path));
                let module_key = manifest_name;
                let deps = meta.as_ref().map(|m| m.deps.clone()).unwrap_or_default();

                tracing::debug!(module = %name, path = %rel_path, deps = deps.len(), "modules: Node module detected");
                upsert_candidate(
                    &mut modules,
                    ModuleCandidate {
                        name,
                        rel_path: rel_path.clone(),
                        build_file: Some(file.path.clone()),
                        build_system: BuildSystem::Node,
                        module_key,
                        deps,
                    },
                );
            }
            "pyproject.toml" => {
                tracing::debug!(path = %file.path, "modules: found pyproject.toml");
                let content = fs::read_to_string(root.join(&file.path))
                    .with_context(|| format!("failed to read {}", file.path))?;
                let rel_path = parent_rel(&file.path);
                let meta = parse_pyproject_toml(&content);
                let name = meta
                    .as_ref()
                    .map(|m| m.artifact_id.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| fallback_module_name(root, &rel_path));
                let module_key = Some(name.clone());
                let deps = meta.as_ref().map(|m| m.deps.clone()).unwrap_or_default();

                tracing::debug!(module = %name, path = %rel_path, deps = deps.len(), "modules: Python pyproject module detected");
                upsert_candidate(
                    &mut modules,
                    ModuleCandidate {
                        name,
                        rel_path: rel_path.clone(),
                        build_file: Some(file.path.clone()),
                        build_system: BuildSystem::Python,
                        module_key,
                        deps,
                    },
                );
            }
            "setup.cfg" => {
                tracing::debug!(path = %file.path, "modules: found setup.cfg");
                let content = fs::read_to_string(root.join(&file.path))
                    .with_context(|| format!("failed to read {}", file.path))?;
                let rel_path = parent_rel(&file.path);
                let meta = parse_setup_cfg(&content);
                let name = meta
                    .as_ref()
                    .map(|m| m.artifact_id.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| fallback_module_name(root, &rel_path));
                let module_key = Some(name.clone());
                let deps = meta.as_ref().map(|m| m.deps.clone()).unwrap_or_default();

                tracing::debug!(module = %name, path = %rel_path, deps = deps.len(), "modules: Python setup.cfg module detected");
                upsert_candidate(
                    &mut modules,
                    ModuleCandidate {
                        name,
                        rel_path: rel_path.clone(),
                        build_file: Some(file.path.clone()),
                        build_system: BuildSystem::Python,
                        module_key,
                        deps,
                    },
                );
            }
            "setup.py" => {
                tracing::debug!(path = %file.path, "modules: found setup.py");
                let rel_path = parent_rel(&file.path);
                let name = fallback_module_name(root, &rel_path);
                tracing::debug!(module = %name, path = %rel_path, "modules: Python setup.py module detected");
                upsert_candidate(
                    &mut modules,
                    ModuleCandidate {
                        name: name.clone(),
                        rel_path: rel_path.clone(),
                        build_file: Some(file.path.clone()),
                        build_system: BuildSystem::Python,
                        module_key: Some(name),
                        deps: Vec::new(),
                    },
                );
            }
            "requirements.txt" => {
                tracing::debug!(path = %file.path, "modules: found requirements.txt");
                let rel_path = parent_rel(&file.path);
                let name = fallback_module_name(root, &rel_path);
                tracing::debug!(module = %name, path = %rel_path, "modules: Python requirements.txt module detected");
                upsert_candidate(
                    &mut modules,
                    ModuleCandidate {
                        name: name.clone(),
                        rel_path: rel_path.clone(),
                        build_file: Some(file.path.clone()),
                        build_system: BuildSystem::Python,
                        module_key: Some(name),
                        deps: Vec::new(),
                    },
                );
            }
            _ => {}
        }
    }

    modules.sort_by(|a, b| a.rel_path.cmp(&b.rel_path).then(a.name.cmp(&b.name)));
    tracing::debug!(modules = modules.len(), "modules: detection complete");
    Ok(modules)
}

fn build_file_priority(file_name: Option<&str>) -> i32 {
    let Some(f) = file_name else {
        return 0;
    };
    let name = f.rsplit('/').next().unwrap_or(f);
    match name {
        "pom.xml" | "build.gradle" | "build.gradle.kts" | "package.json" => 10,
        "pyproject.toml" => 4,
        "setup.cfg" => 3,
        "setup.py" => 2,
        "requirements.txt" => 1,
        _ => 0,
    }
}

pub(super) fn upsert_candidate(modules: &mut Vec<ModuleCandidate>, candidate: ModuleCandidate) {
    if let Some(existing) = modules
        .iter_mut()
        .find(|m| m.rel_path == candidate.rel_path)
    {
        let new_pri = build_file_priority(candidate.build_file.as_deref());
        let old_pri = build_file_priority(existing.build_file.as_deref());

        if new_pri > old_pri
            || (new_pri == old_pri
                && candidate.build_file.is_some()
                && existing.build_file.is_none())
        {
            existing.build_file = candidate.build_file;
            existing.name = candidate.name;
            existing.build_system = candidate.build_system;
            existing.module_key = candidate.module_key;
        }
        existing.deps.extend(candidate.deps);
        existing.deps.sort();
        existing.deps.dedup();
    } else {
        modules.push(candidate);
    }
}

pub(super) fn ensure_unassigned_source_module(
    candidates: &mut Vec<ModuleCandidate>,
    source_files: &[SourceFileInfo],
    root: &Path,
) {
    if source_files.is_empty() {
        return;
    }

    let has_unassigned = source_files
        .iter()
        .any(|sf| find_owner_module(candidates, &sf.path).is_none());
    if has_unassigned {
        upsert_candidate(
            candidates,
            ModuleCandidate {
                name: fallback_module_name(root, "."),
                rel_path: ".".into(),
                build_file: None,
                build_system: BuildSystem::None,
                module_key: None,
                deps: Vec::new(),
            },
        );
    }
}

pub(super) fn find_owner_module<'a>(
    modules: &'a [ModuleCandidate],
    file_path: &str,
) -> Option<&'a str> {
    modules
        .iter()
        .filter(|module| is_under(file_path, &module.rel_path))
        .max_by_key(|module| {
            if module.rel_path == "." {
                0
            } else {
                module.rel_path.len()
            }
        })
        .map(|module| module.rel_path.as_str())
}

pub(super) fn detect_build_system(modules: &[ModuleInfo]) -> BuildSystem {
    let has_maven = modules.iter().any(|m| {
        m.build_file
            .as_deref()
            .is_some_and(|f| f.ends_with("pom.xml"))
    });
    let has_gradle = modules.iter().any(|m| {
        m.build_file
            .as_deref()
            .is_some_and(|f| f.ends_with("build.gradle") || f.ends_with("build.gradle.kts"))
    });
    if has_maven {
        BuildSystem::Maven
    } else if has_gradle {
        BuildSystem::Gradle
    } else {
        BuildSystem::None
    }
}

fn is_under(file_path: &str, rel_path: &str) -> bool {
    rel_path == "." || file_path == rel_path || file_path.starts_with(&format!("{rel_path}/"))
}

fn fallback_module_name(root: &Path, rel_path: &str) -> String {
    if rel_path == "." {
        return root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("root")
            .to_string();
    }
    rel_path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(rel_path)
        .to_string()
}
