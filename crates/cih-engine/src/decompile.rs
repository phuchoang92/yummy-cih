//! JAR decompile pre-step: run Vineflower/CFR/jadx on user-configured JARs before
//! the analyze parse phase, then inject the resulting `.java` files as ordinary
//! source so the call graph flows through them.
//!
//! If `tool_jar` is not set (or the file is missing), Vineflower and CFR are
//! downloaded automatically from GitHub releases into `.cih/tools/`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};
use rayon::prelude::*;

use crate::decompile_config::DecompileConfig;

// ── Download metadata ─────────────────────────────────────────────────────────

struct ToolRelease {
    filename: &'static str,
    url: &'static str,
}

fn tool_release(tool: &str) -> Option<ToolRelease> {
    match tool {
        "vineflower" => Some(ToolRelease {
            filename: "vineflower-1.12.0.jar",
            url: "https://github.com/Vineflower/vineflower/releases/download/1.12.0/vineflower-1.12.0.jar",
        }),
        "cfr" => Some(ToolRelease {
            filename: "cfr-0.152.jar",
            url: "https://github.com/leibnitz27/cfr/releases/download/0.152/cfr-0.152.jar",
        }),
        _ => None,
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

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
    let cih_dir = repo.join(".cih");

    let mut stats = DecompileStats {
        jars_found: jars.len(),
        ..Default::default()
    };

    if jars.is_empty() {
        tracing::info!("no JARs matched decompile config — skipping");
        return Ok((vec![], stats));
    }

    // Resolve (and auto-download if needed) the tool JAR once — before the
    // parallel loop so we don't race on the download.
    let tool = if config.tool.is_empty() { "vineflower" } else { config.tool.as_str() };
    let resolved_tool_jar: Option<String> = if tool != "jadx" {
        Some(
            ensure_tool_jar(config, tool, &cih_dir)
                .with_context(|| format!("resolving decompiler tool '{tool}'"))?
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        None
    };

    let threads = safe_parallel_jobs(tool);
    tracing::info!(threads, tool, "decompile parallelism");

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap_or_else(|_| rayon::ThreadPoolBuilder::new().build().unwrap());

    let results: Vec<(PathBuf, Result<usize>, bool)> = pool.install(|| {
        jars.par_iter().map(|jar| {
            let stem = jar.file_stem().and_then(|n| n.to_str()).unwrap_or("jar");
            let out_dir = cache_root.join(stem);
            if is_cache_valid(jar, &out_dir) {
                let count = count_java_files(&out_dir);
                return (out_dir, Ok(count), true);
            }
            // Stale or missing — wipe and redecompile.
            let _ = std::fs::remove_dir_all(&out_dir);
            let result = run_one_jar(jar, &out_dir, config, resolved_tool_jar.as_deref());
            if result.is_ok() {
                write_jarinfo(jar, &out_dir);
            }
            let count = match &result {
                Ok(n) => *n,
                Err(_) => 0,
            };
            (out_dir, result.map(|_| count), false)
        }).collect()
    });

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

// ── Parallelism ───────────────────────────────────────────────────────────────

/// Compute how many JVM decompile processes to run in parallel.
///
/// Each JVM instance (Vineflower) uses ~512 MB; CFR is lighter at ~256 MB.
/// We cap by both available RAM and CPU count, leaving 20% RAM headroom for
/// the rest of the system.
fn safe_parallel_jobs(tool: &str) -> usize {
    let mb_per_jvm: u64 = if tool == "cfr" { 256 } else { 512 };
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);

    let ram_limit = available_ram_mb()
        .map(|ram| {
            let headroom = ram * 80 / 100; // keep 20% free
            ((headroom / mb_per_jvm) as usize).max(1)
        })
        .unwrap_or(2); // conservative fallback when RAM is unreadable

    let jobs = cpus.min(ram_limit);
    tracing::debug!(cpus, ram_limit, mb_per_jvm, jobs, "decompile parallelism calculated");
    jobs.max(1)
}

/// Available RAM in MB. Returns `None` if it cannot be determined.
fn available_ram_mb() -> Option<u64> {
    available_ram_mb_impl()
}

#[cfg(target_os = "linux")]
fn available_ram_mb_impl() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn available_ram_mb_impl() -> Option<u64> {
    let pages: u64 = sysctl_u64("vm.page_free_count")?;
    let page_size: u64 = sysctl_u64("hw.pagesize")?;
    Some(pages * page_size / (1024 * 1024))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn available_ram_mb_impl() -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &str) -> Option<u64> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", name])
        .output()
        .ok()?;
    std::str::from_utf8(&out.stdout).ok()?.trim().parse().ok()
}

// ── Tool resolution + auto-download ──────────────────────────────────────────

/// Return the path to the tool JAR, downloading it if needed.
///
/// Resolution order:
/// 1. `config.tool_jar` if set and the file exists → use as-is
/// 2. `config.tool_jar` if set but file missing → error (user explicitly pointed at a bad path)
/// 3. `tool_jar` not set → auto-download to `<cih_dir>/tools/<filename>`
fn ensure_tool_jar(config: &DecompileConfig, tool: &str, cih_dir: &Path) -> Result<PathBuf> {
    // Case 1 & 2: user explicitly configured a path
    if let Some(configured) = &config.tool_jar {
        let path = PathBuf::from(configured);
        if path.exists() {
            return Ok(path);
        }
        bail!(
            "tool_jar = {configured:?} does not exist — \
             fix the path in cih.decompile.toml or remove it to enable auto-download"
        );
    }

    // Case 3: auto-download
    let release = tool_release(tool).ok_or_else(|| {
        anyhow::anyhow!(
            "no auto-download available for tool {tool:?} — \
             set `tool_bin` in cih.decompile.toml"
        )
    })?;

    let tools_dir = cih_dir.join("tools");
    std::fs::create_dir_all(&tools_dir)
        .with_context(|| format!("create tools dir: {}", tools_dir.display()))?;

    let dest = tools_dir.join(release.filename);
    if dest.exists() {
        tracing::debug!(path = %dest.display(), "tool JAR already downloaded");
        return Ok(dest);
    }

    download_tool(release.url, &dest, release.filename)?;
    Ok(dest)
}

/// Download `url` to `dest`, writing via a `.tmp` file for atomicity.
fn download_tool(url: &str, dest: &Path, display_name: &str) -> Result<()> {
    eprint!("  Downloading {display_name} ... ");
    let _ = std::io::stderr().flush();

    let tmp = dest.with_extension("jar.tmp");

    let response = ureq::get(url)
        .call()
        .with_context(|| format!("HTTP GET {url}"))?;

    let content_len = response
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok());

    let mut reader = response.into_reader();
    let mut file = std::fs::File::create(&tmp)
        .with_context(|| format!("create temp file: {}", tmp.display()))?;

    std::io::copy(&mut reader, &mut file)
        .with_context(|| format!("write {display_name}"))?;

    std::fs::rename(&tmp, dest)
        .with_context(|| format!("rename {} → {}", tmp.display(), dest.display()))?;

    let size_kb = content_len.unwrap_or_else(|| dest.metadata().map(|m| m.len()).unwrap_or(0)) / 1024;
    eprintln!("done ({size_kb} KB) → {}", dest.display());
    tracing::info!(path = %dest.display(), "decompiler tool downloaded");
    Ok(())
}

// ── Subprocess invocation ─────────────────────────────────────────────────────

fn run_one_jar(
    jar: &Path,
    out_dir: &Path,
    config: &DecompileConfig,
    tool_jar: Option<&str>,
) -> Result<usize> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("create decompile output dir: {}", out_dir.display()))?;

    let tool = if config.tool.is_empty() { "vineflower" } else { config.tool.as_str() };

    let status = match tool {
        "vineflower" => {
            let jar_path = tool_jar.context(
                "internal: tool_jar should have been resolved before run_one_jar (vineflower)",
            )?;
            std::process::Command::new("java")
                .args([
                    "-jar",
                    jar_path,
                    "--log-level=error",
                    jar.to_str().unwrap_or(""),
                    out_dir.to_str().unwrap_or(""),
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .with_context(|| format!("spawn java -jar {jar_path} (vineflower)"))?
        }
        "cfr" => {
            let jar_path = tool_jar.context(
                "internal: tool_jar should have been resolved before run_one_jar (cfr)",
            )?;
            std::process::Command::new("java")
                .args([
                    "-jar",
                    jar_path,
                    jar.to_str().unwrap_or(""),
                    "--outputdir",
                    out_dir.to_str().unwrap_or(""),
                    "--silent",
                    "true",
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .with_context(|| format!("spawn java -jar {jar_path} (cfr)"))?
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
        other => bail!(
            "unknown decompiler tool {other:?} — expected \"vineflower\", \"cfr\", or \"jadx\""
        ),
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

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Write `{size}_{mtime}` into `<out_dir>/.jarinfo` so we can detect JAR changes.
fn write_jarinfo(jar: &Path, out_dir: &Path) {
    let Ok(meta) = std::fs::metadata(jar) else { return };
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(out_dir.join(".jarinfo"), format!("{size}_{mtime}"));
}

/// True if the decompile output is present and matches the JAR's current size+mtime.
fn is_cache_valid(jar: &Path, out_dir: &Path) -> bool {
    if !has_java_files(out_dir) {
        return false;
    }
    let Ok(meta) = std::fs::metadata(jar) else { return false };
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let expected = format!("{size}_{mtime}");
    std::fs::read_to_string(out_dir.join(".jarinfo"))
        .map(|s| s.trim() == expected)
        .unwrap_or(false)
}

fn has_java_files(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
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
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_java_files(&path, cb);
        } else if path.extension().and_then(|e| e.to_str()) == Some("java") {
            cb(&path);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_valid_after_write_jarinfo() {
        let tmp = tempfile::tempdir().unwrap();
        let jar = tmp.path().join("mfa-core-2.1.jar");
        std::fs::write(&jar, b"fake jar bytes").unwrap();
        let out_dir = tmp.path().join("mfa-core-2.1");
        std::fs::create_dir_all(&out_dir).unwrap();
        std::fs::write(out_dir.join("Foo.java"), b"class Foo {}").unwrap();
        assert!(!is_cache_valid(&jar, &out_dir), "no .jarinfo yet");
        write_jarinfo(&jar, &out_dir);
        assert!(is_cache_valid(&jar, &out_dir), "valid after write");
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

    #[test]
    fn ensure_tool_jar_errors_on_missing_configured_path() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::decompile_config::DecompileConfig {
            tool: "vineflower".into(),
            tool_jar: Some("/nonexistent/path/vineflower.jar".into()),
            ..Default::default()
        };
        let err = ensure_tool_jar(&cfg, "vineflower", tmp.path()).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn ensure_tool_jar_returns_existing_configured_path() {
        let tmp = tempfile::tempdir().unwrap();
        let jar = tmp.path().join("vineflower.jar");
        std::fs::write(&jar, b"fake").unwrap();
        let cfg = crate::decompile_config::DecompileConfig {
            tool: "vineflower".into(),
            tool_jar: Some(jar.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let resolved = ensure_tool_jar(&cfg, "vineflower", tmp.path()).unwrap();
        assert_eq!(resolved, jar);
    }

    #[test]
    fn tool_release_known_tools() {
        assert!(tool_release("vineflower").is_some());
        assert!(tool_release("cfr").is_some());
        assert!(tool_release("jadx").is_none());
        assert!(tool_release("unknown").is_none());
    }
}
