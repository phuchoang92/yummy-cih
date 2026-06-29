//! User-configured JAR decompile settings, loaded from `cih.decompile.toml`.
//!
//! Users list one or more directory+prefix pairs. Before each `analyze` run,
//! all JARs in the configured dirs whose filenames start with the given prefix
//! are decompiled to `.cih/decompiled/<cache_key>/` and injected into the
//! source-file scan as ordinary Java source.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Top-level decompile configuration.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DecompileConfig {
    /// Decompiler tool: `"vineflower"` (recommended), `"cfr"`, or `"jadx"`.
    pub tool: String,
    /// Absolute path to `cfr.jar` (required when `tool = "cfr"`).
    pub tool_jar: Option<String>,
    /// Absolute path to the `jadx` binary (required when `tool = "jadx"`).
    pub tool_bin: Option<String>,
    /// Directory where decompiled `.java` files are cached.
    /// Default: `<repo>/.cih/decompiled`.
    pub cache_dir: Option<String>,
    /// JAR directories and prefix filters to decompile.
    pub sources: Vec<DecompileSource>,
}

/// One directory + prefix pair.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DecompileSource {
    /// Directory to scan for JARs (absolute or repo-relative).
    pub dir: String,
    /// Filename prefix filter — only JARs whose filename starts with this are decompiled.
    /// Example: `"mfa-"` matches `mfa-core-2.1.jar` but skips `commons-lang3.jar`.
    pub prefix: String,
}

impl DecompileConfig {
    const CONFIG_FILE: &'static str = "cih.decompile.toml";

    /// Load from `<repo>/cih.decompile.toml`. Returns `Default` if the file is absent.
    pub fn load_or_default(repo: &Path) -> Self {
        let path = repo.join(Self::CONFIG_FILE);
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        match toml::from_str::<Self>(&content) {
            Ok(cfg) => cfg,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "Failed to parse cih.decompile.toml — using defaults"
                );
                Self::default()
            }
        }
    }

    /// Persist the config to `<repo>/cih.decompile.toml`.
    pub fn save(&self, repo: &Path) -> anyhow::Result<()> {
        let path = repo.join(Self::CONFIG_FILE);
        let content = toml::to_string_pretty(self)
            .map_err(|e| anyhow::anyhow!("failed to serialize decompile config: {e}"))?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    /// Resolved cache directory (creates it if absent).
    pub fn resolved_cache_dir(&self, repo: &Path) -> PathBuf {
        let dir = self
            .cache_dir
            .as_deref()
            .unwrap_or(".cih/decompiled");
        let path = if std::path::Path::new(dir).is_absolute() {
            PathBuf::from(dir)
        } else {
            repo.join(dir)
        };
        let _ = std::fs::create_dir_all(&path);
        path
    }

    /// Collect all JAR file paths that match the configured `dir`+`prefix` pairs.
    ///
    /// Paths that do not exist or cannot be read are silently skipped.
    pub fn collect_jars(&self, repo: &Path) -> Vec<PathBuf> {
        let mut jars = Vec::new();
        for source in &self.sources {
            let dir = expand_tilde(&source.dir);
            let dir = if Path::new(&dir).is_absolute() {
                PathBuf::from(&dir)
            } else {
                repo.join(&dir)
            };
            let Ok(entries) = std::fs::read_dir(&dir) else {
                tracing::warn!(dir = %dir.display(), "decompile source dir not found — skipping");
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jar") {
                    continue;
                }
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if filename.starts_with(source.prefix.as_str()) {
                    jars.push(path);
                }
            }
        }
        jars.sort();
        jars
    }

    /// True if there is at least one configured source.
    pub fn is_enabled(&self) -> bool {
        !self.sources.is_empty()
    }
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}/{rest}", home.to_string_lossy());
        }
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_jars_filters_by_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("mfa-core-2.1.jar"), b"fake").unwrap();
        std::fs::write(dir.join("mfa-auth-1.0.jar"), b"fake").unwrap();
        std::fs::write(dir.join("commons-lang3.jar"), b"fake").unwrap();

        let cfg = DecompileConfig {
            sources: vec![DecompileSource {
                dir: dir.to_string_lossy().to_string(),
                prefix: "mfa-".into(),
            }],
            ..Default::default()
        };
        let jars = cfg.collect_jars(dir);
        assert_eq!(jars.len(), 2);
        assert!(jars.iter().all(|j| j.file_name().unwrap().to_str().unwrap().starts_with("mfa-")));
    }

    #[test]
    fn load_or_default_returns_default_when_file_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = DecompileConfig::load_or_default(tmp.path());
        assert!(cfg.sources.is_empty());
    }

    #[test]
    fn save_and_reload_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = DecompileConfig {
            tool: "cfr".into(),
            tool_jar: Some("/opt/cfr.jar".into()),
            sources: vec![
                DecompileSource { dir: "target/lib".into(), prefix: "mfa-".into() },
            ],
            ..Default::default()
        };
        cfg.save(tmp.path()).unwrap();
        let loaded = DecompileConfig::load_or_default(tmp.path());
        assert_eq!(loaded.tool, "cfr");
        assert_eq!(loaded.sources.len(), 1);
        assert_eq!(loaded.sources[0].prefix, "mfa-");
    }
}
