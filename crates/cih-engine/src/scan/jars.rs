//! Phase 4.4a — JAR discovery (metadata only). Catalogs dependency JARs into
//! `RepoMap.jars` without touching the tree-sitter source walk.
//!
//! Sources (in priority order):
//!  1. Project-local `lib/`, `libs/` directories.
//!  2. `.workspace-dependencies/` at the repo root.
//!  3. Maven local repository (`~/.m2/repository/`) — targeted per known dep.
//!  4. Gradle user-home files cache (`~/.gradle/caches/modules-*/files-*/`).

use std::collections::BTreeSet;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::{fs, io};

use cih_core::JarInfo;

/// Collect all discoverable JARs for the repo, sorted by path.
///
/// `repo_deps` — union of `group_id:artifact_id` pairs from build files.
/// `own_prefix` — the project's own group prefix (e.g. `"com.example"`); used to
/// set `is_own`. Empty string → nothing is marked own.
pub(super) fn discover_jars(root: &Path, repo_deps: &[String], own_prefix: &str) -> Vec<JarInfo> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut jars: Vec<JarInfo> = Vec::new();

    // 1 + 2: project-local directories
    for rel in &["lib", "libs", ".workspace-dependencies"] {
        let dir = root.join(rel);
        if dir.is_dir() {
            walk_jars(&dir, 6, own_prefix, &mut seen, &mut jars);
        }
    }

    // 3: Maven local repository — targeted lookup per known dep
    if let Some(m2) = maven_local_repo() {
        for dep in repo_deps {
            let Some((group, artifact)) = dep.split_once(':') else {
                continue;
            };
            if let Some(jar_path) = find_in_m2(&m2, group, artifact) {
                let key = jar_path.to_string_lossy().into_owned();
                if seen.insert(key.clone()) {
                    jars.push(JarInfo {
                        path: key,
                        group_id: Some(group.to_string()),
                        artifact: Some(artifact.to_string()),
                        is_own: is_own(group, own_prefix),
                        classes: count_jar_classes(&jar_path).unwrap_or(0),
                    });
                }
            }
        }
    }

    // 4: Gradle user-home files cache — targeted lookup per known dep
    if let Some(gradle_files) = gradle_files_dir() {
        for dep in repo_deps {
            let Some((group, artifact)) = dep.split_once(':') else {
                continue;
            };
            if let Some(jar_path) = find_in_gradle(&gradle_files, group, artifact) {
                let key = jar_path.to_string_lossy().into_owned();
                if seen.insert(key.clone()) {
                    jars.push(JarInfo {
                        path: key,
                        group_id: Some(group.to_string()),
                        artifact: Some(artifact.to_string()),
                        is_own: is_own(group, own_prefix),
                        classes: count_jar_classes(&jar_path).unwrap_or(0),
                    });
                }
            }
        }
    }

    jars.sort_by(|a, b| a.path.cmp(&b.path));
    jars
}

// --- internal helpers ---

fn walk_jars(
    dir: &Path,
    max_depth: u32,
    own_prefix: &str,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<JarInfo>,
) {
    walk_jars_inner(dir, 0, max_depth, own_prefix, seen, out);
}

fn walk_jars_inner(
    dir: &Path,
    depth: u32,
    max_depth: u32,
    own_prefix: &str,
    seen: &mut BTreeSet<String>,
    out: &mut Vec<JarInfo>,
) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jars_inner(&path, depth + 1, max_depth, own_prefix, seen, out);
        } else if is_candidate_jar(&path) {
            let key = path.to_string_lossy().into_owned();
            if seen.insert(key.clone()) {
                let (group_id, artifact) = group_artifact_from_path(&path);
                let is_own_flag = group_id
                    .as_deref()
                    .map(|g| is_own(g, own_prefix))
                    .unwrap_or(false);
                out.push(JarInfo {
                    path: key,
                    group_id,
                    artifact,
                    is_own: is_own_flag,
                    classes: count_jar_classes(&path).unwrap_or(0),
                });
            }
        }
    }
}

fn is_candidate_jar(path: &Path) -> bool {
    if path.extension().and_then(|e| e.to_str()) != Some("jar") {
        return false;
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    !stem.ends_with("-sources") && !stem.ends_with("-javadoc") && !stem.ends_with("-tests")
}

/// Extract (group_id, artifact) from a path.
///
/// Maven layout: `…/repository/{group/path}/{artifact}/{version}/{artifact}-{version}.jar`
/// Gradle layout: `…/files-2.1/{group}/{artifact}/{version}/{hash}/{artifact}-{version}.jar`
///
/// Falls back to (None, stem) for paths that don't match either pattern.
fn group_artifact_from_path(path: &Path) -> (Option<String>, Option<String>) {
    let components: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();

    // Maven: anchor on "repository" segment
    if let Some(repo_pos) = components.iter().rposition(|&c| c == "repository") {
        let after = &components[repo_pos + 1..];
        // after = [group_dirs..., artifact, version, filename]
        if after.len() >= 3 {
            let n = after.len();
            let artifact = after[n - 3];
            let group_path = after[..n - 3].join(".");
            if !group_path.is_empty() && !artifact.is_empty() {
                return (Some(group_path), Some(artifact.to_string()));
            }
        }
    }

    // Gradle: anchor on "files-*" segment
    // after files-*: [group, artifact, version, hash, filename]
    if let Some(files_pos) = components.iter().rposition(|&c| c.starts_with("files-")) {
        let after = &components[files_pos + 1..];
        if after.len() >= 5 {
            let group = after[0];
            let artifact = after[1];
            if !group.is_empty() && !artifact.is_empty() {
                return (Some(group.to_string()), Some(artifact.to_string()));
            }
        }
    }

    // Fallback: stem only (e.g. `guava-33.0-jre.jar` → artifact = `guava-33.0-jre`)
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(str::to_string);
    (None, stem)
}

/// Find the primary JAR for `group_id:artifact_id` in the Maven local repo.
fn find_in_m2(m2: &Path, group_id: &str, artifact_id: &str) -> Option<PathBuf> {
    let group_path = group_id.replace('.', "/");
    let artifact_dir = m2.join(&group_path).join(artifact_id);
    if !artifact_dir.is_dir() {
        return None;
    }
    // Walk one level for version directories; pick the last (alphabetically highest ≈ latest).
    let mut best: Option<PathBuf> = None;
    for entry in fs::read_dir(&artifact_dir).ok()?.flatten() {
        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
            if let Some(jar) = find_primary_jar_in(&entry.path()) {
                best = Some(jar);
            }
        }
    }
    best
}

/// Find the primary JAR for `group_id:artifact_id` in the Gradle files cache.
/// Layout: `files-*/{group}/{artifact}/{version}/{hash}/{artifact}-{version}.jar`
fn find_in_gradle(gradle_files: &Path, group_id: &str, artifact_id: &str) -> Option<PathBuf> {
    let artifact_dir = gradle_files.join(group_id).join(artifact_id);
    if !artifact_dir.is_dir() {
        return None;
    }
    for version_entry in fs::read_dir(&artifact_dir).ok()?.flatten() {
        if !version_entry
            .file_type()
            .map(|ft| ft.is_dir())
            .unwrap_or(false)
        {
            continue;
        }
        for hash_entry in fs::read_dir(version_entry.path()).ok()?.flatten() {
            if hash_entry
                .file_type()
                .map(|ft| ft.is_dir())
                .unwrap_or(false)
            {
                if let Some(jar) = find_primary_jar_in(&hash_entry.path()) {
                    return Some(jar);
                }
            }
        }
    }
    None
}

fn find_primary_jar_in(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| is_candidate_jar(p))
}

fn is_own(group_id: &str, own_prefix: &str) -> bool {
    !own_prefix.is_empty()
        && (group_id == own_prefix || group_id.starts_with(&format!("{own_prefix}.")))
}

/// Count `.class` entries inside a JAR (reads the ZIP central directory, no decompression).
fn count_jar_classes(path: &Path) -> io::Result<u64> {
    let file = fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(BufReader::new(file))?;
    let mut count = 0u64;
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i) {
            if entry.name().ends_with(".class") {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn maven_local_repo() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("M2_REPO") {
        let p = PathBuf::from(&explicit);
        if p.is_dir() {
            return Some(p);
        }
    }
    let p = home_dir()?.join(".m2").join("repository");
    p.is_dir().then_some(p)
}

/// Return the `files-*` subdirectory of the first `modules-*` entry in the Gradle cache.
fn gradle_files_dir() -> Option<PathBuf> {
    let base = if let Ok(explicit) = std::env::var("GRADLE_USER_HOME") {
        PathBuf::from(explicit)
    } else {
        home_dir()?.join(".gradle")
    };
    let caches = base.join("caches");
    if !caches.is_dir() {
        return None;
    }
    // Find the first modules-* subdir, then its files-* child.
    let modules_dir = fs::read_dir(&caches)
        .ok()?
        .flatten()
        .filter(|e| {
            e.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                && e.file_name().to_string_lossy().starts_with("modules-")
        })
        .map(|e| e.path())
        .next()?;
    fs::read_dir(&modules_dir)
        .ok()?
        .flatten()
        .filter(|e| {
            e.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                && e.file_name().to_string_lossy().starts_with("files-")
        })
        .map(|e| e.path())
        .next()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

// --- tests ---

#[cfg(test)]
mod tests;

