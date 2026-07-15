//! Persistent per-repo / per-user settings for `analyze`, `discover`, `wiki`.
//!
//! Layered defaults so the option flags don't have to be retyped every run.
//! Precedence, highest wins:
//!
//! ```text
//! CLI flag  >  env var  >  <repo>/cih.toml  >  ~/.cih/config.toml  >  built-in default
//! ```
//!
//! The file format mirrors the existing per-repo TOMLs (`cih.scope.toml`,
//! `cih.taint.toml`): partial files are valid, a malformed file logs a warning and
//! is skipped (fail-soft, like [`cih_taint::load_taint_rules`]).

use std::path::{Path, PathBuf};

use serde::Deserialize;

// ── Built-in defaults ───────────────────────────────────────────────────────
// Kept here (not as clap `default_value`) so absence of a flag is detectable and
// the resolver — not clap — owns the fallback.

pub const DEFAULT_COMMUNITY_STRATEGY: &str = "package";
pub const DEFAULT_FEATURE_STRATEGY: &str = "package";
pub const DEFAULT_FEATURE_LLM_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_FEATURE_LLM_MAX_TOKENS: u32 = 2048;
pub const DEFAULT_FEATURE_LLM_TIMEOUT_SECS: u64 = 60;

// Embed clusterer knobs (--feature-strategy embed). Must match `EmbedClusterConfig::default`.
pub const DEFAULT_EMBED_SIMILARITY_THRESHOLD: f32 = 0.65;
pub const DEFAULT_EMBED_KNN: usize = 15;
pub const DEFAULT_EMBED_LEIDEN_RESOLUTION: f64 = 0.8;

pub const DEFAULT_WIKI_LLM_PROVIDER: &str = "openai-compatible";
pub const DEFAULT_WIKI_LLM_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_WIKI_LLM_MODEL: &str = "";
pub const DEFAULT_WIKI_LLM_MAX_TOKENS: u32 = 600;
pub const DEFAULT_WIKI_LLM_TIMEOUT_SECS: u64 = 30;
pub const DEFAULT_WIKI_LLM_RETRIES: u32 = 2;
pub const DEFAULT_WIKI_LLM_CONCURRENCY: usize = 8;
pub const DEFAULT_WIKI_LANGUAGE: &str = "en";
pub const DEFAULT_WIKI_MODE: &str = "graph";
pub const DEFAULT_WIKI_GROUPING: &str = "package";

// ── Source tracking ─────────────────────────────────────────────────────────

/// Which layer supplied a resolved value (for `cih config show`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    HomeConfig,
    RepoConfig,
    Env,
    Flag,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::HomeConfig => "~/.cih/config.toml",
            Source::RepoConfig => "cih.toml",
            Source::Env => "env",
            Source::Flag => "flag",
        }
    }
}

/// A resolved value plus the layer it came from.
#[derive(Debug, Clone)]
pub struct Resolved<T> {
    pub value: T,
    pub source: Source,
}

/// Resolve one option through the precedence ladder. `env` is `None` for options
/// with no environment binding.
pub fn resolve<T>(
    flag: Option<T>,
    env: Option<T>,
    repo: Option<T>,
    home: Option<T>,
    default: T,
) -> Resolved<T> {
    if let Some(value) = flag {
        Resolved {
            value,
            source: Source::Flag,
        }
    } else if let Some(value) = env {
        Resolved {
            value,
            source: Source::Env,
        }
    } else if let Some(value) = repo {
        Resolved {
            value,
            source: Source::RepoConfig,
        }
    } else if let Some(value) = home {
        Resolved {
            value,
            source: Source::HomeConfig,
        }
    } else {
        Resolved {
            value: default,
            source: Source::Default,
        }
    }
}

/// Resolve an "enable" bool flag (clap presence flag: true or absent). Config can
/// turn it on; a present flag also turns it on. Known v1 limitation: a config
/// `true` cannot be turned off from the CLI (these are all enable-flags).
pub fn resolve_bool(flag: bool, repo: Option<bool>, home: Option<bool>) -> Resolved<bool> {
    if flag {
        Resolved {
            value: true,
            source: Source::Flag,
        }
    } else if let Some(v) = repo {
        Resolved {
            value: v,
            source: Source::RepoConfig,
        }
    } else if let Some(v) = home {
        Resolved {
            value: v,
            source: Source::HomeConfig,
        }
    } else {
        Resolved {
            value: false,
            source: Source::Default,
        }
    }
}

// ── Settings schema ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalyzeSettings {
    pub languages: Option<Vec<String>>,
    pub skip_xml_integration: Option<bool>,
    pub include_decompiled: Option<bool>,
    /// Explicit CXF servlet base path (e.g. `/rest`) prepended to `<jaxrs:server>`
    /// route paths. Overrides auto-detection when set.
    pub cxf_base_path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscoverSettings {
    pub community_strategy: Option<String>,
    pub resolution: Option<f64>,
    pub min_community_size: Option<usize>,
    pub max_trace_depth: Option<usize>,
    pub max_processes: Option<usize>,
    pub max_branching: Option<usize>,
    pub min_trace_confidence: Option<f32>,
    pub feature_strategy: Option<String>,
    pub feature_llm_provider: Option<String>,
    pub feature_llm_model: Option<String>,
    pub feature_llm_base_url: Option<String>,
    pub feature_llm_api_key_env: Option<String>,
    pub feature_llm_max_tokens: Option<u32>,
    pub feature_llm_timeout_secs: Option<u64>,
    pub embed_similarity_threshold: Option<f32>,
    pub embed_knn: Option<usize>,
    pub embed_leiden_resolution: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WikiSettings {
    pub llm: Option<bool>,
    pub llm_provider: Option<String>,
    pub llm_base_url: Option<String>,
    pub llm_model: Option<String>,
    pub llm_api_key_env: Option<String>,
    pub llm_max_tokens: Option<u32>,
    pub llm_timeout_secs: Option<u64>,
    pub llm_retries: Option<u32>,
    pub llm_concurrency: Option<usize>,
    pub wiki_language: Option<String>,
    pub wiki_mode: Option<String>,
    pub grouping: Option<String>,
    pub html: Option<bool>,
    pub incremental: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CihSettings {
    pub analyze: AnalyzeSettings,
    pub discover: DiscoverSettings,
    pub wiki: WikiSettings,
}

impl CihSettings {
    /// Parse a settings file. `None` when the file is absent; a parse error logs a
    /// warning and returns `None` so the run falls back to lower layers.
    fn load_file(path: &Path) -> Option<CihSettings> {
        let content = std::fs::read_to_string(path).ok()?;
        match toml::from_str(&content) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "invalid cih settings file — ignoring");
                None
            }
        }
    }
}

/// The home (`~/.cih/config.toml`) and repo (`<repo>/cih.toml`) layers, kept
/// separate so `config show` can attribute each value to its source.
#[derive(Debug, Clone, Default)]
pub struct Layers {
    pub home: CihSettings,
    pub repo: CihSettings,
}

/// `~/.cih/config.toml`, if `HOME` is set.
pub fn home_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cih").join("config.toml"))
}

/// `<repo>/cih.toml`.
pub fn repo_config_path(repo: &Path) -> PathBuf {
    repo.join("cih.toml")
}

impl Layers {
    pub fn load(repo: &Path) -> Layers {
        let home = home_config_path()
            .and_then(|p| CihSettings::load_file(&p))
            .unwrap_or_default();
        let repo = CihSettings::load_file(&repo_config_path(repo)).unwrap_or_default();
        Layers { home, repo }
    }
}

// ── Per-command flag resolution ─────────────────────────────────────────────
// One function per command with config-backed options. Each takes the raw flag
// values (absence detectable) and the loaded layers, and returns the effective
// values. The CLI dispatch stays thin, and precedence is unit-testable here.

pub mod resolve;
pub mod show;
pub use resolve::*;
pub use show::*;

#[cfg(test)]
mod tests;
