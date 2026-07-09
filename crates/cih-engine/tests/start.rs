use anyhow::Result;
use cih_engine_lib::start::*;
use std::path::{Path, PathBuf};

/// Test runner that records commands instead of executing them.
#[allow(dead_code)]
pub struct TestCommandRunner {
    pub recorded: std::cell::RefCell<Vec<(String, String)>>,
}

impl Default for TestCommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl TestCommandRunner {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            recorded: std::cell::RefCell::new(Vec::new()),
        }
    }
}

impl CommandRunner for TestCommandRunner {
    fn run(&self, label: &str, command: &str) -> Result<()> {
        self.recorded
            .borrow_mut()
            .push((label.to_string(), command.to_string()));
        Ok(())
    }
}

// ── validate_repo_path ──────────────────────────────────────────────

#[test]
fn start_validate_repo_path_accepts_existing_dir() {
    let tmp = std::env::temp_dir();
    let result = validate_repo_path(&tmp);
    assert!(
        result.is_ok(),
        "temp dir should exist and be canonicalizable: {:?}",
        result
    );
    let canonical = result.unwrap();
    assert!(canonical.is_absolute(), "canonical path must be absolute");
}

#[test]
fn start_validate_repo_path_rejects_missing() {
    let path = Path::new("/tmp/nonexistent_cih_test_dir_12345");
    let result = validate_repo_path(path);
    assert!(result.is_err(), "missing path should return Err");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("repository path does not exist"),
        "error message must mention 'repository path does not exist': {}",
        err
    );
}

// ── default_repo_name ───────────────────────────────────────────────

#[test]
fn start_default_repo_name_from_path() {
    // Simple basename with hyphens should pass through.
    let name = default_repo_name(Path::new("/home/user/my-cool-repo"));
    assert_eq!(name, "my-cool-repo");

    // Uppercase → lowercase.
    let name = default_repo_name(Path::new("/tmp/SomeRepo"));
    assert_eq!(name, "somerepo");

    // Special characters become hyphens, then trimmed.
    let name = default_repo_name(Path::new("/opt/Repo_Name!"));
    assert_eq!(name, "repo-name");

    // Leading/trailing special chars trimmed.
    let name = default_repo_name(Path::new("/tmp/__my__repo__"));
    assert_eq!(name, "my--repo");
}

// ── LlmChoice ───────────────────────────────────────────────────────

#[test]
fn start_llm_none_produces_no_key() {
    assert_eq!(
        LlmChoice::None.env_var(),
        None,
        "LlmChoice::None must yield no API key env var"
    );
}

#[test]
fn start_llm_choices_single_provider() {
    assert_eq!(
        LlmChoice::DeepSeek.env_var(),
        Some("DEEPSEEK_API_KEY"),
        "DeepSeek maps to DEEPSEEK_API_KEY"
    );
    assert_eq!(
        LlmChoice::Gemini.env_var(),
        Some("GEMINI_API_KEY"),
        "Gemini maps to GEMINI_API_KEY"
    );
    assert_eq!(
        LlmChoice::Anthropic.env_var(),
        Some("ANTHROPIC_API_KEY"),
        "Anthropic maps to ANTHROPIC_API_KEY"
    );
    assert_eq!(
        LlmChoice::OpenAI.env_var(),
        Some("OPENAI_API_KEY"),
        "OpenAI maps to OPENAI_API_KEY"
    );

    // Sanity: DeepSeek should NOT produce the Gemini key.
    assert_ne!(LlmChoice::DeepSeek.env_var(), LlmChoice::Gemini.env_var());
}

// ── build_command_plan ──────────────────────────────────────────────

#[test]
fn start_builds_full_command_plan() {
    let cfg = StartConfig {
        repo: Some(PathBuf::from("/tmp/test-repo")),
        repo_name: Some("test-repo".into()),
        workspace: PathBuf::from("."),
        index_mode: IndexMode::AnalyzeAll,
        do_discover: true,
        do_embed: true,
        do_wiki: true,
        do_docs: true,
        llm: LlmChoice::None,
        dry_run: false,
        non_interactive: false,
        postgres_password: None,
    };

    let plan = build_command_plan(&cfg);

    let labels: Vec<&str> = plan.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "pull",
            "up",
            "ps",
            "scan",
            "analyze",
            "discover",
            "embed",
            "wiki",
            "docs-viewer"
        ],
        "full plan must produce ordered labels"
    );
    assert_eq!(plan.len(), 9, "full plan = 3 always + 6 optional");
}

#[test]
fn start_build_command_plan_never_includes_down_v() {
    // Enumerate every mode + all flag combinations to ensure no "down -v" leak.
    let modes = [
        IndexMode::ScanOnly,
        IndexMode::AnalyzeAll,
        IndexMode::Modules(vec!["core".into()]),
    ];
    let bools = [false, true];

    for mode in &modes {
        for do_discover in bools {
            for do_embed in bools {
                for do_wiki in bools {
                    for do_docs in bools {
                        let cfg = StartConfig {
                            repo: Some(PathBuf::from("/tmp/r")),
                            repo_name: None,
                            workspace: PathBuf::from("."),
                            index_mode: mode.clone(),
                            do_discover,
                            do_embed,
                            do_wiki,
                            do_docs,
                            llm: LlmChoice::None,
                            dry_run: false,
                            non_interactive: false,
                            postgres_password: None,
                        };
                        let plan = build_command_plan(&cfg);
                        for cmd in &plan {
                            assert!(
                                !cmd.command.to_lowercase().contains("down -v")
                                    && !cmd.command.to_lowercase().contains("down  -v"),
                                "command '{}' must NOT contain 'down -v': {}",
                                cmd.label,
                                cmd.command
                            );
                        }
                    }
                }
            }
        }
    }
}
