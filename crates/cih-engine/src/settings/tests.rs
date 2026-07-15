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
    assert_eq!(
        r.feature_llm_provider.as_deref(),
        Some("gemini"),
        "home fills gap"
    );
    assert_eq!(r.max_processes, Some(7));
    assert_eq!(r.resolution, Some(2.5));
    assert_eq!(r.feature_strategy, DEFAULT_FEATURE_STRATEGY);
    assert_eq!(r.feature_llm_base_url, DEFAULT_FEATURE_LLM_BASE_URL);
    assert_eq!(r.feature_llm_max_tokens, DEFAULT_FEATURE_LLM_MAX_TOKENS);
    assert!(
        r.embed_knn.is_none(),
        "embed knobs stay unset for downstream defaults"
    );

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
