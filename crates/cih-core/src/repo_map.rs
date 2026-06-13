//! `RepoMap` — the output of the Phase 3 discovery scan (`cih-engine scan`).
//! Deterministic, parse-free: a module/JAR breakdown the user reviews before
//! choosing what to index.

use serde::{Deserialize, Serialize};

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
}

/// Detected build system for the repo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildSystem {
    Maven,
    Gradle,
    None,
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
