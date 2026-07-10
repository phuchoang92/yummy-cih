//! `cih-engine wiki` — settings layering, then wiki generation.

use anyhow::Result;

use crate::llm;
use crate::settings;
use crate::wiki::{run_wiki, WikiConfig};

use super::args::WikiArgs;

pub fn run(args: WikiArgs) -> Result<()> {
    // Layer flags over <repo>/cih.toml and ~/.cih/config.toml (see settings.rs).
    let layers = settings::Layers::load(&args.repo);
    let r = settings::resolve_wiki(
        settings::WikiFlagInputs {
            llm: args.llm || args.llm_enrich,
            llm_provider: args.llm_provider,
            llm_base_url: args.llm_base_url,
            llm_model: args.llm_model,
            llm_api_key_env: args.llm_api_key_env,
            llm_max_tokens: args.llm_max_tokens,
            llm_timeout_secs: args.llm_timeout_secs,
            llm_retries: args.llm_retries,
            llm_concurrency: args.llm_concurrency,
            wiki_language: args.wiki_language,
            wiki_mode: args.wiki_mode,
            grouping: args.grouping,
            html: args.html,
            incremental: args.incremental,
        },
        &layers,
    );

    run_wiki(WikiConfig {
        repo: args.repo,
        out: args.out,
        run_llm: r.run_llm,
        llm: llm::LlmCallConfig {
            provider: r.llm_provider.parse()?,
            base_url: r.llm_base_url,
            model: r.llm_model,
            api_key_env: r.llm_api_key_env,
            max_tokens: r.llm_max_tokens,
            timeout_secs: r.llm_timeout_secs,
            retries: r.llm_retries,
        },
        llm_provider_config: args.llm_provider_config,
        evidence_paths: args.evidence,
        llm_concurrency: r.llm_concurrency,
        llm_debug_evidence: args.llm_debug_evidence,
        llm_dry_run: args.llm_dry_run,
        wiki_language: r.wiki_language,
        wiki_mode: r.wiki_mode.parse()?,
        grouping: r.grouping.parse()?,
        html: r.html,
        incremental: r.incremental,
        save_evidence: args.save_evidence,
        filter_community: args.filter_community,
        max_communities: args.max_communities,
        filter_feature: args.filter_feature,
        filter_route: args.filter_route,
        json: args.json,
        check_only: args.check,
        since_ref: args.since,
        stage_and_swap: args.stage_and_swap,
    })
}
