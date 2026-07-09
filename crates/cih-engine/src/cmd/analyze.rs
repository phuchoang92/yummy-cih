//! `cih-engine analyze` — settings layering, then the analyze pipeline.

use anyhow::{Context, Result};

use crate::analyze::{run_analyze, AnalyzeFlags};
use crate::settings;

use super::args::AnalyzeArgs;

pub fn run(args: AnalyzeArgs) -> Result<()> {
    let repo = match args.repo {
        Some(r) => r,
        None => std::env::current_dir().with_context(|| {
            "failed to determine current working directory — pass an explicit repo path or run from a valid directory"
        })?,
    };
    // Layer flags over <repo>/cih.toml and ~/.cih/config.toml (see settings.rs).
    let layers = settings::Layers::load(&repo);
    let resolved = settings::resolve_analyze(
        settings::AnalyzeFlagInputs {
            languages: args.languages,
            skip_xml_integration: args.skip_xml_integration,
            include_decompiled: args.include_decompiled,
            cxf_base_path: args.cxf_base_path,
        },
        &layers,
    );
    run_analyze(
        repo,
        AnalyzeFlags {
            all: args.all,
            modules: args.modules,
            include: args.include,
            exclude: args.exclude,
            include_decompiled: resolved.include_decompiled,
            scope: args.scope,
            json: args.json,
            falkor_url: args.db.falkor_url,
            graph_key: args.db.graph_key,
            no_load: args.db.no_load,
            no_cache: args.no_cache,
            skip_xml_integration: resolved.skip_xml_integration,
            languages: resolved.languages,
            cxf_base_path: resolved.cxf_base_path,
        },
    )
}
