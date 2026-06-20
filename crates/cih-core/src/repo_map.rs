//! `RepoMap` — the output of the Phase 3 discovery scan (`cih-engine scan`).
//! Deterministic, parse-free: a module/JAR breakdown the user reviews before
//! choosing what to index.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Coarse structural hint about how a repo is organised.
/// Auto-detected during `scan` from module count and file size; can be
/// overridden by writing it manually into `repo-map.json`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchitectureHint {
    #[default]
    Unknown,
    Monolith,
    Microservice,
    EventDriven,
    Batch,
}

/// Detect a coarse architecture hint from repo metrics.
/// Rules:
/// - >500 files + >3 build modules → `Monolith`
/// - <100 files → `Microservice`
/// - Otherwise `Unknown`
pub fn auto_detect_architecture(repo_map: &RepoMap) -> ArchitectureHint {
    let total = if repo_map.total_source_files > 0 {
        repo_map.total_source_files
    } else {
        repo_map.total_java_files
    };
    if total > 500 && repo_map.modules.len() > 3 {
        return ArchitectureHint::Monolith;
    }
    if total < 100 {
        return ArchitectureHint::Microservice;
    }
    ArchitectureHint::Unknown
}

/// A repository's module/JAR breakdown produced by the scan (no tree-sitter).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMap {
    /// Absolute path of the scanned repo root.
    pub root: String,
    pub build_system: BuildSystem,
    pub total_java_files: u64,
    /// Approx LOC across `.java` (newline count — no parsing).
    pub total_loc: u64,
    pub modules: Vec<ModuleInfo>,
    /// Dependency JARs found (own vs third-party); see Phase 3 §3.1b.
    pub jars: Vec<JarInfo>,
    /// Dirs holding already-decompiled sources (e.g. `.workspace-dependencies/`).
    pub decompiled_dirs: Vec<String>,
    /// Coarse structural hint — auto-detected during scan or manually set.
    #[serde(default)]
    pub architecture_hint: ArchitectureHint,
    /// Total source files across all registered languages (Java + TypeScript + Python + …).
    #[serde(default)]
    pub total_source_files: u64,
    /// Per-language source file counts, e.g. `{"java": 120, "typescript": 45, "python": 12}`.
    #[serde(default)]
    pub per_language: BTreeMap<String, u64>,
}

/// Detected build system for the repo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildSystem {
    Maven,
    Gradle,
    None,
    Node,
    Python,
    Mixed,
}

/// One build unit (Maven/Gradle module, or a pseudo-module fallback).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleInfo {
    /// Module name (artifactId, Gradle project name, or dir name).
    pub name: String,
    /// Repo-relative path of the module root.
    pub rel_path: String,
    /// `pom.xml` / `build.gradle[.kts]` if present.
    pub build_file: Option<String>,
    pub java_files: u64,
    pub loc: u64,
    /// Top-level Java packages found in the module.
    pub packages: Vec<String>,
    pub spring: SpringSignal,
    /// Sibling modules this one depends on (best-effort from pom/gradle).
    pub depends_on: Vec<String>,
    /// Detected framework/runtime identifiers, e.g. `["spring", "nestjs"]`.
    #[serde(default)]
    pub frameworks: Vec<String>,
}

/// Cheap Spring footprint per module — substring counts over `.java`, no AST.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpringSignal {
    /// `@Controller` + `@RestController`.
    pub controllers: u32,
    pub services: u32,
    pub repositories: u32,
    pub components: u32,
    pub configs: u32,
    pub entities: u32,
    /// `@RequestMapping` + `@*Mapping`.
    pub mappings: u32,
}

/// A dependency JAR discovered during the scan (see Phase 3 §3.1b).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JarInfo {
    pub path: String,
    pub group_id: Option<String>,
    pub artifact: Option<String>,
    /// `true` if the groupId matches a configured `own_group_ids` prefix
    /// (candidate for API-surface extraction; third-party defaults to skip).
    pub is_own: bool,
    /// Number of `.class` entries (cheap zip listing).
    pub classes: u64,
}
