//! `cih config show` presentation — the effective-settings table and the
//! `cih config init` starter TOML.

use super::*;

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
