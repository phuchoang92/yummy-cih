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

/// `analyze` flags that participate in config layering.
#[derive(Debug, Default)]
pub struct AnalyzeFlagInputs {
    pub languages: Vec<String>,
    pub skip_xml_integration: bool,
    pub include_decompiled: bool,
    pub cxf_base_path: Option<String>,
}

/// Effective `analyze` values after `flag > repo > home > default`.
#[derive(Debug)]
pub struct AnalyzeResolved {
    pub languages: Vec<String>,
    pub skip_xml_integration: bool,
    pub include_decompiled: bool,
    pub cxf_base_path: Option<String>,
}

pub fn resolve_analyze(flags: AnalyzeFlagInputs, layers: &Layers) -> AnalyzeResolved {
    let (h, r) = (&layers.home.analyze, &layers.repo.analyze);
    // Empty --language means "unset" → fall back to config, then "all".
    let languages = if flags.languages.is_empty() {
        r.languages
            .clone()
            .or_else(|| h.languages.clone())
            .unwrap_or_default()
    } else {
        flags.languages
    };
    AnalyzeResolved {
        languages,
        skip_xml_integration: resolve_bool(
            flags.skip_xml_integration,
            r.skip_xml_integration,
            h.skip_xml_integration,
        )
        .value,
        include_decompiled: resolve_bool(
            flags.include_decompiled,
            r.include_decompiled,
            h.include_decompiled,
        )
        .value,
        // flag > repo cih.toml > home config (no env binding for this option).
        cxf_base_path: flags
            .cxf_base_path
            .or_else(|| r.cxf_base_path.clone())
            .or_else(|| h.cxf_base_path.clone()),
    }
}

/// `discover` flags that participate in config layering.
#[derive(Debug, Default)]
pub struct DiscoverFlagInputs {
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

/// Effective `discover` values. LLM provider parsing and `LlmCallConfig`
/// construction stay in the CLI layer — this resolves plain values only.
#[derive(Debug)]
pub struct DiscoverResolved {
    pub community_strategy: String,
    pub feature_strategy: String,
    pub resolution: Option<f64>,
    pub min_community_size: Option<usize>,
    pub max_trace_depth: Option<usize>,
    pub max_processes: Option<usize>,
    pub max_branching: Option<usize>,
    pub min_trace_confidence: Option<f32>,
    pub feature_llm_provider: Option<String>,
    /// Empty string means "use the provider default model".
    pub feature_llm_model: String,
    pub feature_llm_base_url: String,
    pub feature_llm_api_key_env: Option<String>,
    pub feature_llm_max_tokens: u32,
    pub feature_llm_timeout_secs: u64,
    /// Embed clusterer knobs stay `Option` so unset falls through to
    /// `EmbedClusterConfig` defaults inside discover.
    pub embed_similarity_threshold: Option<f32>,
    pub embed_knn: Option<usize>,
    pub embed_leiden_resolution: Option<f64>,
}

pub fn resolve_discover(flags: DiscoverFlagInputs, layers: &Layers) -> DiscoverResolved {
    let (h, r) = (&layers.home.discover, &layers.repo.discover);
    DiscoverResolved {
        community_strategy: resolve(
            flags.community_strategy,
            None,
            r.community_strategy.clone(),
            h.community_strategy.clone(),
            DEFAULT_COMMUNITY_STRATEGY.to_string(),
        )
        .value,
        feature_strategy: resolve(
            flags.feature_strategy,
            None,
            r.feature_strategy.clone(),
            h.feature_strategy.clone(),
            DEFAULT_FEATURE_STRATEGY.to_string(),
        )
        .value,
        resolution: flags.resolution.or(r.resolution).or(h.resolution),
        min_community_size: flags
            .min_community_size
            .or(r.min_community_size)
            .or(h.min_community_size),
        max_trace_depth: flags
            .max_trace_depth
            .or(r.max_trace_depth)
            .or(h.max_trace_depth),
        max_processes: flags.max_processes.or(r.max_processes).or(h.max_processes),
        max_branching: flags.max_branching.or(r.max_branching).or(h.max_branching),
        min_trace_confidence: flags
            .min_trace_confidence
            .or(r.min_trace_confidence)
            .or(h.min_trace_confidence),
        feature_llm_provider: flags
            .feature_llm_provider
            .or_else(|| r.feature_llm_provider.clone())
            .or_else(|| h.feature_llm_provider.clone()),
        feature_llm_model: flags
            .feature_llm_model
            .or_else(|| r.feature_llm_model.clone())
            .or_else(|| h.feature_llm_model.clone())
            .unwrap_or_default(),
        feature_llm_base_url: resolve(
            flags.feature_llm_base_url,
            None,
            r.feature_llm_base_url.clone(),
            h.feature_llm_base_url.clone(),
            DEFAULT_FEATURE_LLM_BASE_URL.to_string(),
        )
        .value,
        feature_llm_api_key_env: flags
            .feature_llm_api_key_env
            .or_else(|| r.feature_llm_api_key_env.clone())
            .or_else(|| h.feature_llm_api_key_env.clone()),
        feature_llm_max_tokens: resolve(
            flags.feature_llm_max_tokens,
            None,
            r.feature_llm_max_tokens,
            h.feature_llm_max_tokens,
            DEFAULT_FEATURE_LLM_MAX_TOKENS,
        )
        .value,
        feature_llm_timeout_secs: resolve(
            flags.feature_llm_timeout_secs,
            None,
            r.feature_llm_timeout_secs,
            h.feature_llm_timeout_secs,
            DEFAULT_FEATURE_LLM_TIMEOUT_SECS,
        )
        .value,
        embed_similarity_threshold: flags
            .embed_similarity_threshold
            .or(r.embed_similarity_threshold)
            .or(h.embed_similarity_threshold),
        embed_knn: flags.embed_knn.or(r.embed_knn).or(h.embed_knn),
        embed_leiden_resolution: flags
            .embed_leiden_resolution
            .or(r.embed_leiden_resolution)
            .or(h.embed_leiden_resolution),
    }
}

/// `wiki` flags that participate in config layering. `llm` is the already-OR'd
/// pair of `--llm` and the deprecated `--llm-enrich`.
#[derive(Debug, Default)]
pub struct WikiFlagInputs {
    pub llm: bool,
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
    pub html: bool,
    pub incremental: bool,
}

/// Effective `wiki` values after layering.
#[derive(Debug)]
pub struct WikiResolved {
    pub run_llm: bool,
    pub llm_provider: String,
    pub llm_base_url: String,
    pub llm_model: String,
    pub llm_api_key_env: Option<String>,
    pub llm_max_tokens: u32,
    pub llm_timeout_secs: u64,
    pub llm_retries: u32,
    pub llm_concurrency: usize,
    pub wiki_language: String,
    pub wiki_mode: String,
    pub grouping: String,
    pub html: bool,
    pub incremental: bool,
}

pub fn resolve_wiki(flags: WikiFlagInputs, layers: &Layers) -> WikiResolved {
    let (h, r) = (&layers.home.wiki, &layers.repo.wiki);
    WikiResolved {
        run_llm: resolve_bool(flags.llm, r.llm, h.llm).value,
        llm_provider: resolve(
            flags.llm_provider,
            None,
            r.llm_provider.clone(),
            h.llm_provider.clone(),
            DEFAULT_WIKI_LLM_PROVIDER.to_string(),
        )
        .value,
        llm_base_url: resolve(
            flags.llm_base_url,
            None,
            r.llm_base_url.clone(),
            h.llm_base_url.clone(),
            DEFAULT_WIKI_LLM_BASE_URL.to_string(),
        )
        .value,
        llm_model: resolve(
            flags.llm_model,
            None,
            r.llm_model.clone(),
            h.llm_model.clone(),
            DEFAULT_WIKI_LLM_MODEL.to_string(),
        )
        .value,
        llm_api_key_env: flags
            .llm_api_key_env
            .or_else(|| r.llm_api_key_env.clone())
            .or_else(|| h.llm_api_key_env.clone()),
        llm_max_tokens: resolve(
            flags.llm_max_tokens,
            None,
            r.llm_max_tokens,
            h.llm_max_tokens,
            DEFAULT_WIKI_LLM_MAX_TOKENS,
        )
        .value,
        llm_timeout_secs: resolve(
            flags.llm_timeout_secs,
            None,
            r.llm_timeout_secs,
            h.llm_timeout_secs,
            DEFAULT_WIKI_LLM_TIMEOUT_SECS,
        )
        .value,
        llm_retries: resolve(
            flags.llm_retries,
            None,
            r.llm_retries,
            h.llm_retries,
            DEFAULT_WIKI_LLM_RETRIES,
        )
        .value,
        llm_concurrency: resolve(
            flags.llm_concurrency,
            None,
            r.llm_concurrency,
            h.llm_concurrency,
            DEFAULT_WIKI_LLM_CONCURRENCY,
        )
        .value,
        wiki_language: resolve(
            flags.wiki_language,
            None,
            r.wiki_language.clone(),
            h.wiki_language.clone(),
            DEFAULT_WIKI_LANGUAGE.to_string(),
        )
        .value,
        wiki_mode: resolve(
            flags.wiki_mode,
            None,
            r.wiki_mode.clone(),
            h.wiki_mode.clone(),
            DEFAULT_WIKI_MODE.to_string(),
        )
        .value,
        grouping: resolve(
            flags.grouping,
            None,
            r.grouping.clone(),
            h.grouping.clone(),
            DEFAULT_WIKI_GROUPING.to_string(),
        )
        .value,
        html: resolve_bool(flags.html, r.html, h.html).value,
        incremental: resolve_bool(flags.incremental, r.incremental, h.incremental).value,
    }
}

/// One line in `cih config show`: which section/key, the effective value, and where
/// it came from. No CLI flags are involved here, so the source is repo/home/default.
#[derive(Debug, Clone)]
pub struct ShowRow {
    pub section: &'static str,
    pub key: &'static str,
    pub value: String,
    pub source: Source,
}

/// Effective value + source for every config-backed option, for `cih config show`.
/// Values are formatted for display; options with no built-in value show a `(…)`
/// placeholder and source `Default`.
pub fn effective_rows(layers: &Layers) -> Vec<ShowRow> {
    let (ha, ra) = (&layers.home.analyze, &layers.repo.analyze);
    let (hd, rd) = (&layers.home.discover, &layers.repo.discover);
    let (hw, rw) = (&layers.home.wiki, &layers.repo.wiki);
    let mut rows = Vec::new();

    // A concrete-default option: resolve to its display string + source.
    fn req<T: ToString>(
        section: &'static str,
        key: &'static str,
        repo: Option<T>,
        home: Option<T>,
        default: String,
    ) -> ShowRow {
        let r = resolve(
            None,
            None,
            repo.map(|v| v.to_string()),
            home.map(|v| v.to_string()),
            default,
        );
        ShowRow {
            section,
            key,
            value: r.value,
            source: r.source,
        }
    }
    // A pass-through option with no built-in value: show a placeholder when unset.
    fn opt<T: ToString>(
        section: &'static str,
        key: &'static str,
        repo: Option<T>,
        home: Option<T>,
        unset: &str,
    ) -> ShowRow {
        if let Some(v) = repo {
            ShowRow {
                section,
                key,
                value: v.to_string(),
                source: Source::RepoConfig,
            }
        } else if let Some(v) = home {
            ShowRow {
                section,
                key,
                value: v.to_string(),
                source: Source::HomeConfig,
            }
        } else {
            ShowRow {
                section,
                key,
                value: unset.to_string(),
                source: Source::Default,
            }
        }
    }
    let bool_row = |section, key, repo: Option<bool>, home: Option<bool>| {
        let r = resolve_bool(false, repo, home);
        ShowRow {
            section,
            key,
            value: r.value.to_string(),
            source: r.source,
        }
    };

    // [analyze]
    rows.push(opt(
        "analyze",
        "languages",
        ra.languages.as_ref().map(|v| v.join(",")),
        ha.languages.as_ref().map(|v| v.join(",")),
        "(all)",
    ));
    rows.push(bool_row(
        "analyze",
        "skip_xml_integration",
        ra.skip_xml_integration,
        ha.skip_xml_integration,
    ));
    rows.push(bool_row(
        "analyze",
        "include_decompiled",
        ra.include_decompiled,
        ha.include_decompiled,
    ));
    rows.push(opt(
        "analyze",
        "cxf_base_path",
        ra.cxf_base_path.clone(),
        ha.cxf_base_path.clone(),
        "(auto-detect)",
    ));

    // [discover]
    rows.push(req(
        "discover",
        "community_strategy",
        rd.community_strategy.clone(),
        hd.community_strategy.clone(),
        DEFAULT_COMMUNITY_STRATEGY.to_string(),
    ));
    rows.push(req(
        "discover",
        "feature_strategy",
        rd.feature_strategy.clone(),
        hd.feature_strategy.clone(),
        DEFAULT_FEATURE_STRATEGY.to_string(),
    ));
    rows.push(opt(
        "discover",
        "resolution",
        rd.resolution,
        hd.resolution,
        "(auto)",
    ));
    rows.push(opt(
        "discover",
        "min_community_size",
        rd.min_community_size,
        hd.min_community_size,
        "(auto)",
    ));
    rows.push(opt(
        "discover",
        "max_trace_depth",
        rd.max_trace_depth,
        hd.max_trace_depth,
        "(auto)",
    ));
    rows.push(opt(
        "discover",
        "max_processes",
        rd.max_processes,
        hd.max_processes,
        "(auto)",
    ));
    rows.push(opt(
        "discover",
        "max_branching",
        rd.max_branching,
        hd.max_branching,
        "(auto)",
    ));
    rows.push(opt(
        "discover",
        "min_trace_confidence",
        rd.min_trace_confidence,
        hd.min_trace_confidence,
        "(auto)",
    ));
    rows.push(opt(
        "discover",
        "feature_llm_provider",
        rd.feature_llm_provider.clone(),
        hd.feature_llm_provider.clone(),
        "(none)",
    ));
    rows.push(opt(
        "discover",
        "feature_llm_model",
        rd.feature_llm_model.clone(),
        hd.feature_llm_model.clone(),
        "(provider default)",
    ));
    rows.push(req(
        "discover",
        "feature_llm_base_url",
        rd.feature_llm_base_url.clone(),
        hd.feature_llm_base_url.clone(),
        DEFAULT_FEATURE_LLM_BASE_URL.to_string(),
    ));
    rows.push(opt(
        "discover",
        "feature_llm_api_key_env",
        rd.feature_llm_api_key_env.clone(),
        hd.feature_llm_api_key_env.clone(),
        "(auto-detect)",
    ));
    rows.push(req(
        "discover",
        "feature_llm_max_tokens",
        rd.feature_llm_max_tokens,
        hd.feature_llm_max_tokens,
        DEFAULT_FEATURE_LLM_MAX_TOKENS.to_string(),
    ));
    rows.push(req(
        "discover",
        "feature_llm_timeout_secs",
        rd.feature_llm_timeout_secs,
        hd.feature_llm_timeout_secs,
        DEFAULT_FEATURE_LLM_TIMEOUT_SECS.to_string(),
    ));
    rows.push(req(
        "discover",
        "embed_similarity_threshold",
        rd.embed_similarity_threshold,
        hd.embed_similarity_threshold,
        DEFAULT_EMBED_SIMILARITY_THRESHOLD.to_string(),
    ));
    rows.push(req(
        "discover",
        "embed_knn",
        rd.embed_knn,
        hd.embed_knn,
        DEFAULT_EMBED_KNN.to_string(),
    ));
    rows.push(req(
        "discover",
        "embed_leiden_resolution",
        rd.embed_leiden_resolution,
        hd.embed_leiden_resolution,
        DEFAULT_EMBED_LEIDEN_RESOLUTION.to_string(),
    ));

    // [wiki]
    rows.push(bool_row("wiki", "llm", rw.llm, hw.llm));
    rows.push(req(
        "wiki",
        "llm_provider",
        rw.llm_provider.clone(),
        hw.llm_provider.clone(),
        DEFAULT_WIKI_LLM_PROVIDER.to_string(),
    ));
    rows.push(req(
        "wiki",
        "llm_base_url",
        rw.llm_base_url.clone(),
        hw.llm_base_url.clone(),
        DEFAULT_WIKI_LLM_BASE_URL.to_string(),
    ));
    rows.push(opt(
        "wiki",
        "llm_model",
        rw.llm_model.clone(),
        hw.llm_model.clone(),
        "(provider default)",
    ));
    rows.push(opt(
        "wiki",
        "llm_api_key_env",
        rw.llm_api_key_env.clone(),
        hw.llm_api_key_env.clone(),
        "(auto-detect)",
    ));
    rows.push(req(
        "wiki",
        "llm_max_tokens",
        rw.llm_max_tokens,
        hw.llm_max_tokens,
        DEFAULT_WIKI_LLM_MAX_TOKENS.to_string(),
    ));
    rows.push(req(
        "wiki",
        "llm_timeout_secs",
        rw.llm_timeout_secs,
        hw.llm_timeout_secs,
        DEFAULT_WIKI_LLM_TIMEOUT_SECS.to_string(),
    ));
    rows.push(req(
        "wiki",
        "llm_retries",
        rw.llm_retries,
        hw.llm_retries,
        DEFAULT_WIKI_LLM_RETRIES.to_string(),
    ));
    rows.push(req(
        "wiki",
        "llm_concurrency",
        rw.llm_concurrency,
        hw.llm_concurrency,
        DEFAULT_WIKI_LLM_CONCURRENCY.to_string(),
    ));
    rows.push(req(
        "wiki",
        "wiki_language",
        rw.wiki_language.clone(),
        hw.wiki_language.clone(),
        DEFAULT_WIKI_LANGUAGE.to_string(),
    ));
    rows.push(req(
        "wiki",
        "wiki_mode",
        rw.wiki_mode.clone(),
        hw.wiki_mode.clone(),
        DEFAULT_WIKI_MODE.to_string(),
    ));
    rows.push(req(
        "wiki",
        "grouping",
        rw.grouping.clone(),
        hw.grouping.clone(),
        DEFAULT_WIKI_GROUPING.to_string(),
    ));
    rows.push(bool_row("wiki", "html", rw.html, hw.html));
    rows.push(bool_row(
        "wiki",
        "incremental",
        rw.incremental,
        hw.incremental,
    ));

    rows
}

/// A commented starter `cih.toml` — every config-backed option shown with its
/// built-in default, commented out so an empty file behaves exactly as today.
pub fn starter_toml() -> String {
    format!(
        r#"# CIH settings — defaults for `analyze`, `discover`, and `wiki`.
# Precedence: CLI flag > env var > this file > ~/.cih/config.toml > built-in default.
# Uncomment and edit any line to change a default. See `cih config show`.

[analyze]
# languages = ["java"]            # default: all detected
# skip_xml_integration = false
# include_decompiled = false
# cxf_base_path = "/rest"          # CXF servlet base path for <jaxrs:server> routes; default: auto-detect

[discover]
# community_strategy = "{cs}"      # package | graph
# feature_strategy = "{fs}"        # package | structural | hybrid | llm | embed
# resolution = 1.0                 # graph strategy only
# min_community_size = 2           # graph strategy only
# max_trace_depth = 10
# max_processes = 500
# max_branching = 4
# min_trace_confidence = 0.5
# feature_llm_provider = "gemini"  # deepseek | gemini | anthropic | bedrock | openai-compatible
# feature_llm_model = ""           # provider default when empty
# feature_llm_base_url = "{flbu}"
# feature_llm_api_key_env = ""     # auto-detect when empty
# feature_llm_max_tokens = {flmt}
# feature_llm_timeout_secs = {flts}
# embed_similarity_threshold = {est}  # embed strategy: min cosine similarity for a k-NN edge
# embed_knn = {ekn}                    # embed strategy: neighbors per node
# embed_leiden_resolution = {elr}      # embed strategy: higher = more, smaller clusters

[wiki]
# llm = false
# llm_provider = "{wp}"
# llm_base_url = "{wbu}"
# llm_model = ""                   # provider default when empty
# llm_api_key_env = ""             # auto-detect when empty
# llm_max_tokens = {wmt}
# llm_timeout_secs = {wts}
# llm_retries = {wr}
# llm_concurrency = {wc}
# wiki_language = "{wl}"
# wiki_mode = "{wm}"               # graph | llm-summary | llm-full
# grouping = "{wg}"               # package | graph | llm
# html = false
# incremental = false
"#,
        cs = DEFAULT_COMMUNITY_STRATEGY,
        fs = DEFAULT_FEATURE_STRATEGY,
        flbu = DEFAULT_FEATURE_LLM_BASE_URL,
        flmt = DEFAULT_FEATURE_LLM_MAX_TOKENS,
        flts = DEFAULT_FEATURE_LLM_TIMEOUT_SECS,
        est = DEFAULT_EMBED_SIMILARITY_THRESHOLD,
        ekn = DEFAULT_EMBED_KNN,
        elr = DEFAULT_EMBED_LEIDEN_RESOLUTION,
        wp = DEFAULT_WIKI_LLM_PROVIDER,
        wbu = DEFAULT_WIKI_LLM_BASE_URL,
        wmt = DEFAULT_WIKI_LLM_MAX_TOKENS,
        wts = DEFAULT_WIKI_LLM_TIMEOUT_SECS,
        wr = DEFAULT_WIKI_LLM_RETRIES,
        wc = DEFAULT_WIKI_LLM_CONCURRENCY,
        wl = DEFAULT_WIKI_LANGUAGE,
        wm = DEFAULT_WIKI_MODE,
        wg = DEFAULT_WIKI_GROUPING,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_precedence_and_source() {
        // flag wins over everything
        let r = resolve(Some(1), Some(2), Some(3), Some(4), 0);
        assert_eq!((r.value, r.source), (1, Source::Flag));
        // env over repo/home/default
        let r = resolve::<i32>(None, Some(2), Some(3), Some(4), 0);
        assert_eq!((r.value, r.source), (2, Source::Env));
        // repo over home/default
        let r = resolve::<i32>(None, None, Some(3), Some(4), 0);
        assert_eq!((r.value, r.source), (3, Source::RepoConfig));
        // home over default
        let r = resolve::<i32>(None, None, None, Some(4), 0);
        assert_eq!((r.value, r.source), (4, Source::HomeConfig));
        // nothing set → default
        let r = resolve::<i32>(None, None, None, None, 0);
        assert_eq!((r.value, r.source), (0, Source::Default));
    }

    #[test]
    fn resolve_bool_enable_semantics() {
        assert_eq!(resolve_bool(true, Some(false), None).source, Source::Flag);
        assert!(resolve_bool(false, Some(true), None).value);
        assert_eq!(
            resolve_bool(false, Some(true), None).source,
            Source::RepoConfig
        );
        assert_eq!(
            resolve_bool(false, None, Some(true)).source,
            Source::HomeConfig
        );
        assert!(!resolve_bool(false, None, None).value);
    }

    #[test]
    fn parses_partial_file_with_only_one_section() {
        let toml = r#"
            [discover]
            feature_strategy = "hybrid"
            max_trace_depth = 7
        "#;
        let s: CihSettings = toml::from_str(toml).unwrap();
        assert_eq!(s.discover.feature_strategy.as_deref(), Some("hybrid"));
        assert_eq!(s.discover.max_trace_depth, Some(7));
        assert!(s.discover.community_strategy.is_none());
        assert!(s.analyze.languages.is_none());
        assert!(s.wiki.llm.is_none());
    }

    #[test]
    fn repo_layer_overrides_home_via_resolve() {
        let home = CihSettings {
            discover: DiscoverSettings {
                feature_strategy: Some("package".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let repo = CihSettings {
            discover: DiscoverSettings {
                feature_strategy: Some("hybrid".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let r = resolve(
            None,
            None,
            repo.discover.feature_strategy.clone(),
            home.discover.feature_strategy.clone(),
            DEFAULT_FEATURE_STRATEGY.to_string(),
        );
        assert_eq!(r.value, "hybrid");
        assert_eq!(r.source, Source::RepoConfig);
    }

    #[test]
    fn unknown_key_is_rejected() {
        let toml = r#"
            [discover]
            not_a_real_key = 3
        "#;
        assert!(toml::from_str::<CihSettings>(toml).is_err());
    }

    // ── Per-command resolver precedence ─────────────────────────────────────

    fn layers(repo_toml: &str, home_toml: &str) -> Layers {
        Layers {
            repo: toml::from_str(repo_toml).unwrap(),
            home: toml::from_str(home_toml).unwrap(),
        }
    }

    #[test]
    fn analyze_flag_beats_repo_beats_home() {
        let layers = layers(
            "[analyze]\nlanguages = [\"java\"]\ncxf_base_path = \"/repo\"",
            "[analyze]\nlanguages = [\"python\"]\ncxf_base_path = \"/home\"\nskip_xml_integration = true",
        );
        // Flag set → wins.
        let r = resolve_analyze(
            AnalyzeFlagInputs {
                languages: vec!["go".into()],
                cxf_base_path: Some("/flag".into()),
                ..Default::default()
            },
            &layers,
        );
        assert_eq!(r.languages, vec!["go"]);
        assert_eq!(r.cxf_base_path.as_deref(), Some("/flag"));
        // Flag unset → repo wins over home; bools fall through to home.
        let r = resolve_analyze(AnalyzeFlagInputs::default(), &layers);
        assert_eq!(r.languages, vec!["java"]);
        assert_eq!(r.cxf_base_path.as_deref(), Some("/repo"));
        assert!(r.skip_xml_integration, "home config bool should apply");
        assert!(!r.include_decompiled, "unset everywhere → default false");
    }

    #[test]
    fn analyze_defaults_when_all_layers_empty() {
        let r = resolve_analyze(AnalyzeFlagInputs::default(), &Layers::default());
        assert!(r.languages.is_empty(), "empty = all languages");
        assert!(!r.skip_xml_integration);
        assert!(r.cxf_base_path.is_none());
    }

    #[test]
    fn discover_precedence_and_defaults() {
        let layers = layers(
            "[discover]\ncommunity_strategy = \"graph\"\nmax_processes = 7",
            "[discover]\ncommunity_strategy = \"package\"\nfeature_llm_provider = \"gemini\"\nresolution = 2.5",
        );
        let r = resolve_discover(DiscoverFlagInputs::default(), &layers);
        assert_eq!(r.community_strategy, "graph", "repo beats home");
        assert_eq!(r.feature_llm_provider.as_deref(), Some("gemini"), "home fills gap");
        assert_eq!(r.max_processes, Some(7));
        assert_eq!(r.resolution, Some(2.5));
        assert_eq!(r.feature_strategy, DEFAULT_FEATURE_STRATEGY);
        assert_eq!(r.feature_llm_base_url, DEFAULT_FEATURE_LLM_BASE_URL);
        assert_eq!(r.feature_llm_max_tokens, DEFAULT_FEATURE_LLM_MAX_TOKENS);
        assert!(r.embed_knn.is_none(), "embed knobs stay unset for downstream defaults");

        let r = resolve_discover(
            DiscoverFlagInputs {
                community_strategy: Some("llm".into()),
                max_processes: Some(3),
                ..Default::default()
            },
            &layers,
        );
        assert_eq!(r.community_strategy, "llm", "flag beats repo");
        assert_eq!(r.max_processes, Some(3));
    }

    #[test]
    fn wiki_precedence_and_defaults() {
        let layers = layers(
            "[wiki]\nllm = true\nllm_provider = \"anthropic\"",
            "[wiki]\nllm_model = \"m-home\"\nllm_concurrency = 3",
        );
        let r = resolve_wiki(WikiFlagInputs::default(), &layers);
        assert!(r.run_llm, "repo llm=true applies without the flag");
        assert_eq!(r.llm_provider, "anthropic");
        assert_eq!(r.llm_model, "m-home", "home fills gap");
        assert_eq!(r.llm_concurrency, 3);
        assert_eq!(r.wiki_mode, DEFAULT_WIKI_MODE);
        assert_eq!(r.llm_retries, DEFAULT_WIKI_LLM_RETRIES);

        let r = resolve_wiki(
            WikiFlagInputs {
                llm_provider: Some("deepseek".into()),
                wiki_mode: Some("llm-full".into()),
                html: true,
                ..Default::default()
            },
            &layers,
        );
        assert_eq!(r.llm_provider, "deepseek", "flag beats repo");
        assert_eq!(r.wiki_mode, "llm-full");
        assert!(r.html);
    }
}
