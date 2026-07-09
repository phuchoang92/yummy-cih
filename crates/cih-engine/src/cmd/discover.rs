//! `cih-engine discover` — settings layering + LLM config, then discovery.

use anyhow::Result;

use crate::discover::{run_discover, DiscoverOverrides};
use crate::llm;
use crate::settings;

use super::args::DiscoverArgs;

pub fn run(args: DiscoverArgs) -> Result<()> {
    // Layer flags over <repo>/cih.toml and ~/.cih/config.toml (see settings.rs).
    let layers = settings::Layers::load(&args.repo);
    let r = settings::resolve_discover(
        settings::DiscoverFlagInputs {
            community_strategy: args.community_strategy,
            resolution: args.resolution,
            min_community_size: args.min_community_size,
            max_trace_depth: args.max_trace_depth,
            max_processes: args.max_processes,
            max_branching: args.max_branching,
            min_trace_confidence: args.min_trace_confidence,
            feature_strategy: args.feature_strategy,
            feature_llm_provider: args.feature_llm_provider,
            feature_llm_model: args.feature_llm_model,
            feature_llm_base_url: args.feature_llm_base_url,
            feature_llm_api_key_env: args.feature_llm_api_key_env,
            feature_llm_max_tokens: args.feature_llm_max_tokens,
            feature_llm_timeout_secs: args.feature_llm_timeout_secs,
            embed_similarity_threshold: args.embed_similarity_threshold,
            embed_knn: args.embed_knn,
            embed_leiden_resolution: args.embed_leiden_resolution,
        },
        &layers,
    );

    // Build optional LLM config when a provider is specified.
    let feature_llm = r
        .feature_llm_provider
        .map(|s| s.parse::<llm::LlmProvider>())
        .transpose()?
        .map(|provider| {
            let model = if r.feature_llm_model.is_empty() {
                match provider {
                    llm::LlmProvider::DeepSeek => "deepseek-chat".to_string(),
                    llm::LlmProvider::Gemini => "gemini-2.5-flash".to_string(),
                    llm::LlmProvider::Anthropic => "claude-haiku-4-5-20251001".to_string(),
                    llm::LlmProvider::Bedrock => {
                        "us.anthropic.claude-haiku-4-5-20251001".to_string()
                    }
                    _ => "gpt-4o-mini".to_string(),
                }
            } else {
                r.feature_llm_model.clone()
            };
            llm::LlmCallConfig {
                provider,
                base_url: r.feature_llm_base_url,
                model,
                api_key_env: r.feature_llm_api_key_env,
                max_tokens: r.feature_llm_max_tokens,
                timeout_secs: r.feature_llm_timeout_secs,
                retries: 0,
            }
        });

    run_discover(
        args.repo,
        args.db.falkor_url,
        args.db.graph_key,
        args.db.no_load,
        args.json,
        DiscoverOverrides {
            community_strategy: r.community_strategy,
            resolution: r.resolution,
            min_community_size: r.min_community_size,
            max_trace_depth: r.max_trace_depth,
            max_processes: r.max_processes,
            max_branching: r.max_branching,
            min_trace_confidence: r.min_trace_confidence,
            feature_strategy: r.feature_strategy.parse().unwrap_or_default(),
            feature_llm,
            pg_url: args.pg_url,
            embed_similarity_threshold: r.embed_similarity_threshold,
            embed_knn: r.embed_knn,
            embed_leiden_resolution: r.embed_leiden_resolution,
        },
    )
}
