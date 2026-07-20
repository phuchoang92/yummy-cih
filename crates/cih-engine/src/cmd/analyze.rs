//! `cih-engine analyze` — settings layering, then the analyze pipeline.

use anyhow::{anyhow, Context, Result};

use crate::analyze::{run_analyze, AnalyzeFlags};
use crate::settings;

use super::args::AnalyzeArgs;

/// Reject `--language` values no provider claims. The scan filter matches on
/// exact language id, so an unknown value silently selects **zero files** and
/// produces an empty-but-"successful" index — the failure mode this guards.
/// Validated against the live provider registry, never a hard-coded list.
fn validate_languages(languages: &[String]) -> Result<()> {
    if languages.is_empty() {
        return Ok(());
    }
    let mut supported: Vec<&'static str> = cih_lang::all_providers()
        .iter()
        .map(|provider| provider.language_id())
        .collect();
    supported.sort_unstable();
    let unknown: Vec<&str> = languages
        .iter()
        .map(String::as_str)
        .filter(|language| !supported.contains(language))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(anyhow!(
        "unknown --language value(s): {}. Supported: {}",
        unknown.join(", "),
        supported.join(", ")
    ))
}

pub fn run(args: AnalyzeArgs) -> Result<()> {
    let repo = match args.repo {
        Some(r) => r,
        None => std::env::current_dir().with_context(|| {
            "failed to determine current working directory — pass an explicit repo path or run from a valid directory"
        })?,
    };
    // Layer flags over <repo>/cih.toml and ~/.cih/config.toml (see settings.rs).
    let layers = settings::Layers::load(&repo);
    // Map CLI bool flags to Option<bool> so the resolver can distinguish
    // "explicitly set by user" from "not provided — fall through to config".
    // --skip-xml-integration → Some(true), --no-skip-xml-integration → Some(false),
    // neither → None (config layer wins).
    let skip_xml_integration = match (args.skip_xml_integration, args.no_skip_xml_integration) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    let include_decompiled = match (args.include_decompiled, args.no_include_decompiled) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    let resolved = settings::resolve_analyze(
        settings::AnalyzeFlagInputs {
            languages: args
                .languages
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect(),
            skip_xml_integration,
            include_decompiled,
            cxf_base_path: args.cxf_base_path,
        },
        &layers,
    );
    validate_languages(&resolved.languages)?;
    run_analyze(
        repo,
        AnalyzeFlags {
            all: args.all,
            modules: args.modules.into_iter().filter(|s| !s.is_empty()).collect(),
            include: args.include,
            exclude: args.exclude,
            include_decompiled: resolved.include_decompiled,
            scope: args.scope,
            json: args.json,
            backend: args.db.backend,
            falkor_url: args.db.falkor_url,
            graph_key: args.db.graph_key,
            no_load: args.db.no_load,
            no_cache: args.no_cache,
            skip_xml_integration: resolved.skip_xml_integration,
            languages: resolved.languages,
            route_base_path: resolved.cxf_base_path,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::validate_languages;

    /// An unknown `--language` used to select zero files, so the index came out
    /// empty but "successful". It must fail loudly, and the message must list
    /// what is actually supported.
    #[test]
    fn unknown_languages_are_rejected_with_the_supported_set() {
        let error = validate_languages(&["kotlim".to_string()]).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("kotlim"), "{message}");
        assert!(message.contains("java"), "{message}");
        assert!(message.contains("kotlin"), "{message}");
    }

    #[test]
    fn supported_and_empty_language_filters_pass() {
        validate_languages(&[]).expect("no filter means all languages");
        validate_languages(&["java".to_string(), "typescript".to_string()])
            .expect("registered providers must be accepted");
    }
}
