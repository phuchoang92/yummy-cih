//! JAR decompile pre-step: run CFR or jadx on user-configured JARs before
//! the analyze parse phase, then inject the resulting `.java` files as ordinary
//! source so the call graph flows through them.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;

use crate::decompile_config::DecompileConfig;

/// Statistics returned by `run_decompile_precheck`.
#[derive(Debug, Default)]
pub struct DecompileStats {
    pub jars_found: usize,
    pub jars_cached: usize,
    pub jars_decompiled: usize,
    pub jars_failed: usize,
    pub classes_written: usize,
}

/// Run the decompile pre-step.
///
/// Returns `(output_dirs, stats)` where each path in `output_dirs` is a
/// directory containing `.java` files that should be added to the parse scope.
pub fn run_decompile_precheck(
    repo: &Path,
    config: &DecompileConfig,
) -> Result<(Vec<PathBuf>, DecompileStats)> {
    let jars = config.collect_jars(repo);
    let cache_root = config.resolved_cache_dir(repo);

    let mut stats = DecompileStats {
        jars_found: jars.len(),
        ..Default::default()
    };

    if jars.is_empty() {
        tracing::info!("no JARs matched decompile config — skipping");
        return Ok((vec![], stats));
    }

    let results: Vec<(PathBuf, Result<usize>, bool)> = jars
        .par_iter()
        .map(|jar| {
            let cache_key = jar_cache_key(jar);
            let out_dir = cache_root.join(&cache_key);
            if out_dir.exists() && has_java_files(&out_dir) {
                let count = count_java_files(&out_dir);
                return (out_dir, Ok(count), true);
            }
            let result = run_one_jar(jar, &out_dir, config);
            let count = match &result {
                Ok(n) => *n,
                Err(_) => 0,
            };
            (out_dir, result.map(|_| count), false)
        })
        .collect();

    let mut out_dirs = Vec::new();
    for (out_dir, result, was_cached) in results {
        match result {
            Ok(count) => {
                if was_cached {
                    stats.jars_cached += 1;
                } else {
                    stats.jars_decompiled += 1;
                }
                stats.classes_written += count;
                out_dirs.push(out_dir);
            }
            Err(err) => {
                stats.jars_failed += 1;
                tracing::warn!(error = %err, "JAR decompile failed — skipping");
            }
        }
    }

    Ok((out_dirs, stats))
}

/// Collect all `.java` file paths (relative to `repo`) from decompiled output dirs.
pub fn collect_decompiled_java_files(repo: &Path, out_dirs: &[PathBuf]) -> Vec<String> {
    let mut files = Vec::new();
    for dir in out_dirs {
        walk_java_files(dir, &mut |path| {
            if let Ok(rel) = path.strip_prefix(repo) {
                if let Some(s) = rel.to_str() {
                    files.push(s.to_string());
                }
            }
        });
    }
    files
}

fn run_one_jar(jar: &Path, out_dir: &Path, config: &DecompileConfig) -> Result<usize> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("create decompile output dir: {}", out_dir.display()))?;

    let tool = if config.tool.is_empty() { "cfr" } else { config.tool.as_str() };

    let status = match tool {
        "cfr" => {
            let tool_jar = config
                .tool_jar
                .as_deref()
                .context("cih.decompile.toml: `tool_jar` is required when tool = \"cfr\"")?;
            std::process::Command::new("java")
                .args([
                    "-jar",
                    tool_jar,
                    jar.to_str().unwrap_or(""),
                    "--outputdir",
                    out_dir.to_str().unwrap_or(""),
                    "--silent",
                    "true",
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .with_context(|| format!("spawn java -jar {tool_jar}"))?
        }
        "jadx" => {
            let tool_bin = config
                .tool_bin
                .as_deref()
                .context("cih.decompile.toml: `tool_bin` is required when tool = \"jadx\"")?;
            std::process::Command::new(tool_bin)
                .args([
                    "-d",
                    out_dir.to_str().unwrap_or(""),
                    jar.to_str().unwrap_or(""),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .with_context(|| format!("spawn jadx ({tool_bin})"))?
        }
        other => bail!("unknown decompiler tool {other:?} — expected \"cfr\" or \"jadx\""),
    };

    if !status.success() {
        bail!(
            "decompiler exited with code {:?} for {}",
            status.code(),
            jar.display()
        );
    }

    Ok(count_java_files(out_dir))
}

/// Stable cache key for a JAR based on its size + modification time.
///
/// Using metadata avoids reading the JAR bytes on every run (which would be
/// slow for large JARs). Size+mtime is sufficient for cache invalidation:
/// any meaningful change to the JAR will change at least one of these.
fn jar_cache_key(jar: &Path) -> String {
    let Ok(meta) = std::fs::metadata(jar) else {
        return format!("unknown_{}", jar.file_name().and_then(|n| n.to_str()).unwrap_or("jar"));
    };
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stem = jar
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("jar");
    format!("{stem}_{size}_{mtime}")
}

fn has_java_files(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("java") {
            return true;
        }
        if path.is_dir() && has_java_files(&path) {
            return true;
        }
    }
    false
}

fn count_java_files(dir: &Path) -> usize {
    let mut count = 0;
    walk_java_files(dir, &mut |_| count += 1);
    count
}

fn walk_java_files(dir: &Path, cb: &mut impl FnMut(&Path)) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_java_files(&path, cb);
        } else if path.extension().and_then(|e| e.to_str()) == Some("java") {
            cb(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jar_cache_key_is_stable() {
        let tmp = tempfile::tempdir().unwrap();
        let jar = tmp.path().join("mfa-core-2.1.jar");
        std::fs::write(&jar, b"fake jar bytes").unwrap();
        let k1 = jar_cache_key(&jar);
        let k2 = jar_cache_key(&jar);
        assert_eq!(k1, k2);
        assert!(k1.contains("mfa-core-2.1"));
    }

    #[test]
    fn count_java_files_walks_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("com/example");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("Foo.java"), b"class Foo {}").unwrap();
        std::fs::write(tmp.path().join("Bar.java"), b"class Bar {}").unwrap();
        assert_eq!(count_java_files(tmp.path()), 2);
    }

    #[test]
    fn collect_decompiled_java_files_returns_relative_paths() {
        let repo = tempfile::tempdir().unwrap();
        let out_dir = repo.path().join(".cih/decompiled/test_hash");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("Foo.java"), b"class Foo {}").unwrap();
        let files = collect_decompiled_java_files(repo.path(), &[out_dir]);
        assert_eq!(files.len(), 1);
        assert!(files[0].starts_with(".cih/decompiled/"));
    }
}
