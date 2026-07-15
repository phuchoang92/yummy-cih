//! Command settings resolution — merge CLI flags / env / repo & home TOML /
//! defaults into the effective analyze / discover / wiki settings.

use super::*;

/// `analyze` flags that participate in config layering.
///
/// `skip_xml_integration` and `include_decompiled` are `Option<bool>` so that
/// an explicit `--no-*` flag can override a `true` set in `cih.toml`. `None`
/// means "not supplied on the CLI — fall through to the config layer".
#[derive(Debug, Default)]
pub struct AnalyzeFlagInputs {
    pub languages: Vec<String>,
    pub skip_xml_integration: Option<bool>,
    pub include_decompiled: Option<bool>,
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
        skip_xml_integration: resolve(
            flags.skip_xml_integration,
            None, // no env binding for this option
            r.skip_xml_integration,
            h.skip_xml_integration,
            false,
        )
        .value,
        include_decompiled: resolve(
            flags.include_decompiled,
            None, // no env binding for this option
            r.include_decompiled,
            h.include_decompiled,
            false,
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
