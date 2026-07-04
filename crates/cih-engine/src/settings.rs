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
        Resolved { value, source: Source::Flag }
    } else if let Some(value) = env {
        Resolved { value, source: Source::Env }
    } else if let Some(value) = repo {
        Resolved { value, source: Source::RepoConfig }
    } else if let Some(value) = home {
        Resolved { value, source: Source::HomeConfig }
    } else {
        Resolved { value: default, source: Source::Default }
    }
}

/// Resolve an "enable" bool flag (clap presence flag: true or absent). Config can
/// turn it on; a present flag also turns it on. Known v1 limitation: a config
/// `true` cannot be turned off from the CLI (these are all enable-flags).
pub fn resolve_bool(flag: bool, repo: Option<bool>, home: Option<bool>) -> Resolved<bool> {
    if flag {
        Resolved { value: true, source: Source::Flag }
    } else if let Some(v) = repo {
        Resolved { value: v, source: Source::RepoConfig }
    } else if let Some(v) = home {
        Resolved { value: v, source: Source::HomeConfig }
    } else {
        Resolved { value: false, source: Source::Default }
    }
}

// ── Settings schema ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalyzeSettings {
    pub languages: Option<Vec<String>>,
    pub skip_xml_integration: Option<bool>,
    pub include_decompiled: Option<bool>,
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
        ShowRow { section, key, value: r.value, source: r.source }
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
            ShowRow { section, key, value: v.to_string(), source: Source::RepoConfig }
        } else if let Some(v) = home {
            ShowRow { section, key, value: v.to_string(), source: Source::HomeConfig }
        } else {
            ShowRow { section, key, value: unset.to_string(), source: Source::Default }
        }
    }
    let bool_row = |section, key, repo: Option<bool>, home: Option<bool>| {
        let r = resolve_bool(false, repo, home);
        ShowRow { section, key, value: r.value.to_string(), source: r.source }
    };

    // [analyze]
    rows.push(opt(
        "analyze",
        "languages",
        ra.languages.as_ref().map(|v| v.join(",")),
        ha.languages.as_ref().map(|v| v.join(",")),
        "(all)",
    ));
    rows.push(bool_row("analyze", "skip_xml_integration", ra.skip_xml_integration, ha.skip_xml_integration));
    rows.push(bool_row("analyze", "include_decompiled", ra.include_decompiled, ha.include_decompiled));

    // [discover]
    rows.push(req("discover", "community_strategy", rd.community_strategy.clone(), hd.community_strategy.clone(), DEFAULT_COMMUNITY_STRATEGY.to_string()));
    rows.push(req("discover", "feature_strategy", rd.feature_strategy.clone(), hd.feature_strategy.clone(), DEFAULT_FEATURE_STRATEGY.to_string()));
    rows.push(opt("discover", "resolution", rd.resolution, hd.resolution, "(auto)"));
    rows.push(opt("discover", "min_community_size", rd.min_community_size, hd.min_community_size, "(auto)"));
    rows.push(opt("discover", "max_trace_depth", rd.max_trace_depth, hd.max_trace_depth, "(auto)"));
    rows.push(opt("discover", "max_processes", rd.max_processes, hd.max_processes, "(auto)"));
    rows.push(opt("discover", "max_branching", rd.max_branching, hd.max_branching, "(auto)"));
    rows.push(opt("discover", "min_trace_confidence", rd.min_trace_confidence, hd.min_trace_confidence, "(auto)"));
    rows.push(opt("discover", "feature_llm_provider", rd.feature_llm_provider.clone(), hd.feature_llm_provider.clone(), "(none)"));
    rows.push(opt("discover", "feature_llm_model", rd.feature_llm_model.clone(), hd.feature_llm_model.clone(), "(provider default)"));
    rows.push(req("discover", "feature_llm_base_url", rd.feature_llm_base_url.clone(), hd.feature_llm_base_url.clone(), DEFAULT_FEATURE_LLM_BASE_URL.to_string()));
    rows.push(opt("discover", "feature_llm_api_key_env", rd.feature_llm_api_key_env.clone(), hd.feature_llm_api_key_env.clone(), "(auto-detect)"));
    rows.push(req("discover", "feature_llm_max_tokens", rd.feature_llm_max_tokens, hd.feature_llm_max_tokens, DEFAULT_FEATURE_LLM_MAX_TOKENS.to_string()));
    rows.push(req("discover", "feature_llm_timeout_secs", rd.feature_llm_timeout_secs, hd.feature_llm_timeout_secs, DEFAULT_FEATURE_LLM_TIMEOUT_SECS.to_string()));

    // [wiki]
    rows.push(bool_row("wiki", "llm", rw.llm, hw.llm));
    rows.push(req("wiki", "llm_provider", rw.llm_provider.clone(), hw.llm_provider.clone(), DEFAULT_WIKI_LLM_PROVIDER.to_string()));
    rows.push(req("wiki", "llm_base_url", rw.llm_base_url.clone(), hw.llm_base_url.clone(), DEFAULT_WIKI_LLM_BASE_URL.to_string()));
    rows.push(opt("wiki", "llm_model", rw.llm_model.clone(), hw.llm_model.clone(), "(provider default)"));
    rows.push(opt("wiki", "llm_api_key_env", rw.llm_api_key_env.clone(), hw.llm_api_key_env.clone(), "(auto-detect)"));
    rows.push(req("wiki", "llm_max_tokens", rw.llm_max_tokens, hw.llm_max_tokens, DEFAULT_WIKI_LLM_MAX_TOKENS.to_string()));
    rows.push(req("wiki", "llm_timeout_secs", rw.llm_timeout_secs, hw.llm_timeout_secs, DEFAULT_WIKI_LLM_TIMEOUT_SECS.to_string()));
    rows.push(req("wiki", "llm_retries", rw.llm_retries, hw.llm_retries, DEFAULT_WIKI_LLM_RETRIES.to_string()));
    rows.push(req("wiki", "llm_concurrency", rw.llm_concurrency, hw.llm_concurrency, DEFAULT_WIKI_LLM_CONCURRENCY.to_string()));
    rows.push(req("wiki", "wiki_language", rw.wiki_language.clone(), hw.wiki_language.clone(), DEFAULT_WIKI_LANGUAGE.to_string()));
    rows.push(req("wiki", "wiki_mode", rw.wiki_mode.clone(), hw.wiki_mode.clone(), DEFAULT_WIKI_MODE.to_string()));
    rows.push(req("wiki", "grouping", rw.grouping.clone(), hw.grouping.clone(), DEFAULT_WIKI_GROUPING.to_string()));
    rows.push(bool_row("wiki", "html", rw.html, hw.html));
    rows.push(bool_row("wiki", "incremental", rw.incremental, hw.incremental));

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

[discover]
# community_strategy = "{cs}"      # package | graph
# feature_strategy = "{fs}"        # package | structural | hybrid | llm
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
        assert_eq!(resolve_bool(false, Some(true), None).value, true);
        assert_eq!(resolve_bool(false, Some(true), None).source, Source::RepoConfig);
        assert_eq!(resolve_bool(false, None, Some(true)).source, Source::HomeConfig);
        assert_eq!(resolve_bool(false, None, None).value, false);
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
}
