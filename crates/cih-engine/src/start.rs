use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use dialoguer::{Confirm, Input, Password, Select};

use crate::start_env;

// ── Core types ──────────────────────────────────────────────────────────────

/// LLM provider choice for wiki enrichment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LlmChoice {
    #[default]
    None,
    DeepSeek,
    Gemini,
    Anthropic,
    OpenAI,
}

impl LlmChoice {
    /// Returns the API key environment variable name for this provider.
    pub fn env_var(self) -> Option<&'static str> {
        match self {
            LlmChoice::None => None,
            LlmChoice::DeepSeek => Some("DEEPSEEK_API_KEY"),
            LlmChoice::Gemini => Some("GEMINI_API_KEY"),
            LlmChoice::Anthropic => Some("ANTHROPIC_API_KEY"),
            LlmChoice::OpenAI => Some("OPENAI_API_KEY"),
        }
    }
}

/// Indexing strategy for the interactive start workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexMode {
    /// Only scan — no analyze step.
    ScanOnly,
    /// Analyze all modules.
    AnalyzeAll,
    /// Analyze specific modules by name.
    Modules(Vec<String>),
}

impl Default for IndexMode {
    fn default() -> Self {
        IndexMode::AnalyzeAll
    }
}

/// Configuration assembled from user prompts / arguments before
/// building the command plan.
#[derive(Debug, Clone)]
pub struct StartConfig {
    /// Canonical path to the target repository. Required in non-interactive mode.
    pub repo: Option<PathBuf>,
    /// Human-readable repo name (lowercase alphanumeric with hyphens).
    pub repo_name: Option<String>,
    /// Path to the CIH workspace (this repo).
    pub workspace: PathBuf,
    /// Indexing mode: scan-only, analyze-all, or scoped modules.
    pub index_mode: IndexMode,
    /// Run community discovery after indexing.
    pub do_discover: bool,
    /// Run embedding generation.
    pub do_embed: bool,
    /// Generate wiki docs.
    pub do_wiki: bool,
    /// Launch the docs viewer after wiki.
    pub do_docs: bool,
    /// LLM provider for wiki enrichment.
    pub llm: LlmChoice,
    /// Print the plan without executing.
    pub dry_run: bool,
    /// Skip interactive prompts.
    pub non_interactive: bool,
}

impl Default for StartConfig {
    fn default() -> Self {
        Self {
            repo: None,
            repo_name: None,
            workspace: PathBuf::default(),
            index_mode: IndexMode::default(),
            do_discover: true,
            do_embed: true,
            do_wiki: true,
            do_docs: true,
            llm: LlmChoice::default(),
            dry_run: false,
            non_interactive: false,
        }
    }
}

/// A single command in the execution plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCommand {
    /// Human-readable label (e.g. "pull", "analyze").
    pub label: String,
    /// Shell command string.
    pub command: String,
    /// Whether this command is optional (can be skipped).
    pub optional: bool,
    /// Whether this command destroys data (e.g. `down -v`).
    pub is_destructive: bool,
}

// ── Command execution ───────────────────────────────────────────────────────

/// Trait for running shell commands. Allows swapping real execution for tests.
pub trait CommandRunner {
    /// Execute `command` and return Ok(()) on success.
    fn run(&self, label: &str, command: &str) -> Result<()>;
}

/// Production runner that shells out via `std::process::Command`.
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, label: &str, command: &str) -> Result<()> {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .status()
            .with_context(|| format!("failed to spawn command for [{}]", label))?;

        if !status.success() {
            anyhow::bail!(
                "[{}] command exited with status {}: {}",
                label,
                status.code().unwrap_or(-1),
                command
            );
        }
        Ok(())
    }
}

/// Test runner that records commands instead of executing them.
#[cfg(test)]
pub struct TestCommandRunner {
    pub recorded: std::cell::RefCell<Vec<(String, String)>>,
}

#[cfg(test)]
impl TestCommandRunner {
    pub fn new() -> Self {
        Self {
            recorded: std::cell::RefCell::new(Vec::new()),
        }
    }
}

#[cfg(test)]
impl CommandRunner for TestCommandRunner {
    fn run(&self, label: &str, command: &str) -> Result<()> {
        self.recorded
            .borrow_mut()
            .push((label.to_string(), command.to_string()));
        Ok(())
    }
}

// ── Pure functions ──────────────────────────────────────────────────────────

/// Validate that `path` exists on disk and return its canonical form.
pub fn validate_repo_path(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .map_err(|_| anyhow::anyhow!("repository path does not exist: {}", path.display()))
}

/// Extract a default repo name from a path: basename, lowercased,
/// only alphanumeric characters and hyphens.
pub fn default_repo_name(path: &Path) -> String {
    let raw = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    raw.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Build the ordered command plan from configuration.
///
/// The plan always starts with `pull`, `up -d`, and `ps`. Optional steps
/// (scan / analyze / discover / embed / wiki / docs-viewer) are appended
/// according to the config flags. No command ever includes `down -v`.
pub fn build_command_plan(cfg: &StartConfig) -> Vec<PlannedCommand> {
    let mut cmds = Vec::new();

    // ── Always-run infrastructure commands ──
    cmds.push(PlannedCommand {
        label: "pull".into(),
        command: "docker compose pull".into(),
        optional: false,
        is_destructive: false,
    });
    cmds.push(PlannedCommand {
        label: "up".into(),
        command: "docker compose up -d".into(),
        optional: false,
        is_destructive: false,
    });
    cmds.push(PlannedCommand {
        label: "ps".into(),
        command: "docker compose ps".into(),
        optional: false,
        is_destructive: false,
    });

    // ── Indexing: scan (always recommended before analyze) ──
    match &cfg.index_mode {
        IndexMode::ScanOnly => {
            cmds.push(PlannedCommand {
                label: "scan".into(),
                command: "docker compose run --rm engine scan /repo".into(),
                optional: true,
                is_destructive: false,
            });
        }
        IndexMode::AnalyzeAll => {
            cmds.push(PlannedCommand {
                label: "scan".into(),
                command: "docker compose run --rm engine scan /repo".into(),
                optional: true,
                is_destructive: false,
            });
            cmds.push(PlannedCommand {
                label: "analyze".into(),
                command: "docker compose run --rm engine analyze /repo --all".into(),
                optional: true,
                is_destructive: false,
            });
        }
        IndexMode::Modules(modules) => {
            cmds.push(PlannedCommand {
                label: "scan".into(),
                command: "docker compose run --rm engine scan /repo".into(),
                optional: true,
                is_destructive: false,
            });
            let module_list = modules.join(",");
            cmds.push(PlannedCommand {
                label: "analyze".into(),
                command: format!(
                    "docker compose run --rm engine analyze /repo --module {}",
                    module_list
                ),
                optional: true,
                is_destructive: false,
            });
        }
    }

    if cfg.do_discover {
        cmds.push(PlannedCommand {
            label: "discover".into(),
            command: "docker compose run --rm engine discover /repo".into(),
            optional: true,
            is_destructive: false,
        });
    }

    if cfg.do_embed {
        cmds.push(PlannedCommand {
            label: "embed".into(),
            command: "docker compose run --rm engine embed /repo".into(),
            optional: true,
            is_destructive: false,
        });
    }

    if cfg.do_wiki {
        cmds.push(PlannedCommand {
            label: "wiki".into(),
            command: "docker compose run --rm engine wiki /repo".into(),
            optional: true,
            is_destructive: false,
        });
    }

    if cfg.do_docs {
        cmds.push(PlannedCommand {
            label: "docs-viewer".into(),
            command: "docker compose --profile docs up -d docs-viewer".into(),
            optional: true,
            is_destructive: false,
        });
    }

    cmds
}

/// Check prerequisites before running the command plan.
///
/// Hard failures: workspace/docker-compose.yml missing, repo_path invalid.
/// Soft warnings (never block dry-run): docker compose missing, no .java files.
pub fn run_preflight_checks(workspace: &Path, repo_path: &Path, dry_run: bool) -> Result<()> {
    println!("Running preflight checks...");

    // 1. workspace/docker-compose.yml must exist
    let compose_file = workspace.join("docker-compose.yml");
    if !compose_file.exists() {
        anyhow::bail!(
            "docker-compose.yml not found in workspace: {}",
            workspace.display()
        );
    }
    println!("  ✓ docker-compose.yml found");

    // 2. docker compose version (warning only in dry-run)
    let docker_ok = std::process::Command::new("docker")
        .args(["compose", "version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !docker_ok {
        if dry_run {
            eprintln!("  ⚠ docker compose not available (dry-run continues)");
        } else {
            eprintln!("  ⚠ docker compose not available — commands may fail");
        }
    } else {
        println!("  ✓ docker compose available");
    }

    // 3. repo_path must exist
    let canonical = validate_repo_path(repo_path)?;
    println!("  ✓ repo path exists: {}", canonical.display());

    // 4. Check for at least one .java file (warning only)
    let has_java = dir_contains_java(&canonical);
    if !has_java {
        eprintln!(
            "  ⚠ no .java files found under {} (indexing may produce no artifacts)",
            canonical.display()
        );
    } else {
        println!("  ✓ .java files detected");
    }

    Ok(())
}

fn dir_contains_java(dir: &Path) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension() {
                if ext == "java" {
                    return true;
                }
            }
        } else if path.is_dir() && dir_contains_java(&path) {
            return true;
        }
    }
    false
}

/// Execute a command plan with per-command confirmation.
///
/// In dry-run mode, prints what would happen without executing.
/// In interactive mode, asks for confirmation before each command.
pub fn execute_command_plan(
    plan: &[PlannedCommand],
    runner: &dyn CommandRunner,
    dry_run: bool,
    non_interactive: bool,
) -> Result<()> {
    println!("\nExecuting command plan:");

    for cmd in plan {
        println!("[{}] {}", cmd.label, cmd.command);

        if dry_run {
            println!("  [DRY RUN] would execute, skipping");
            continue;
        }

        let proceed = if non_interactive {
            true
        } else {
            Confirm::new()
                .with_prompt(format!("Run [{}]?", cmd.label))
                .default(true)
                .interact()
                .unwrap_or(false)
        };

        if !proceed {
            println!("  Skipped. Copy/paste: {}", cmd.command);
            continue;
        }

        match runner.run(&cmd.label, &cmd.command) {
            Ok(()) => println!("  ✓ done"),
            Err(e) => {
                eprintln!("  ✗ [{}] failed: {}", cmd.label, e);
                if !cmd.optional {
                    anyhow::bail!("required command [{}] failed — aborting", cmd.label);
                }
                eprintln!("  (optional — continuing)");
            }
        }
    }

    Ok(())
}

// ── Public API ──────────────────────────────────────────────────────────────

/// Run the start command with the given configuration.
pub fn run_start(mut cfg: StartConfig) -> Result<()> {
    if cfg.non_interactive {
        let repo = match &cfg.repo {
            Some(r) => r.clone(),
            None => bail!("Error: --repo is required in --non-interactive mode"),
        };
        let repo_canonical = validate_repo_path(&repo)?;
        let repo_name = cfg
            .repo_name
            .clone()
            .unwrap_or_else(|| default_repo_name(&repo_canonical));

        run_preflight_checks(&cfg.workspace, &repo_canonical, cfg.dry_run)?;

        let plan = build_command_plan(&cfg);

        let llm_key_line = cfg.llm.env_var().map(|key| format!("{}=", key));
        let content = start_env::render_env(&repo_canonical, &repo_name, llm_key_line.as_deref());

        start_env::write_env_file(&cfg.workspace, &content, cfg.dry_run)?;

        println!("Command plan:");
        for cmd in &plan {
            let tag = if cmd.optional { "optional" } else { "required" };
            println!("  [{}] {}: {}", tag, cmd.label, cmd.command);
        }

        let runner = RealCommandRunner;
        execute_command_plan(&plan, &runner, cfg.dry_run, cfg.non_interactive)?;

        Ok(())
    } else {
        // ── Interactive mode ──
        println!("══ CIH Interactive Setup Wizard ══\n");

        // Step 1: Workspace
        let workspace_default = cfg.workspace.display().to_string();
        let workspace: String = Input::new()
            .with_prompt("CIH workspace directory (must contain docker-compose.yml)")
            .default(workspace_default)
            .interact_text()?;
        cfg.workspace = PathBuf::from(&workspace);
        println!();

        // Step 2: Repo path (loop until valid)
        let repo_canonical = loop {
            let repo_input: String = Input::new()
                .with_prompt("Java/Spring repository path (absolute)")
                .interact_text()?;
            let repo = PathBuf::from(&repo_input);
            match validate_repo_path(&repo) {
                Ok(p) => {
                    println!("  ✓ found: {}", p.display());
                    break p;
                }
                Err(e) => eprintln!("  ✗ {}. Try again.", e),
            }
        };
        cfg.repo = Some(repo_canonical.clone());
        println!();

        // Step 3: Repo name
        let default_name = default_repo_name(&repo_canonical);
        let repo_name: String = Input::new()
            .with_prompt("Repository name (URL prefix for docs viewer)")
            .default(default_name)
            .interact_text()?;
        cfg.repo_name = Some(repo_name.clone());
        println!();

        // Step 4: Index mode
        let index_options = &["analyze all modules", "scan only (no analyze)", "specific modules"];
        let index_choice = Select::new()
            .with_prompt("Indexing scope")
            .items(index_options)
            .default(0)
            .interact()?;
        cfg.index_mode = match index_choice {
            0 => IndexMode::AnalyzeAll,
            1 => IndexMode::ScanOnly,
            2 => {
                let modules_input: String = Input::new()
                    .with_prompt("Module names (comma-separated, e.g. payment,order)")
                    .interact_text()?;
                let modules: Vec<String> = modules_input
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if modules.is_empty() {
                    println!("  No modules entered, defaulting to analyze-all.");
                    IndexMode::AnalyzeAll
                } else {
                    IndexMode::Modules(modules)
                }
            }
            _ => IndexMode::AnalyzeAll,
        };
        println!();

        // Step 5: Optional steps
        cfg.do_discover = Confirm::new()
            .with_prompt("Run community discover after indexing?")
            .default(true)
            .interact()?;
        cfg.do_embed = Confirm::new()
            .with_prompt("Run embedding generation? (enables semantic search)")
            .default(false)
            .interact()?;
        cfg.do_wiki = Confirm::new()
            .with_prompt("Generate wiki docs?")
            .default(true)
            .interact()?;
        if cfg.do_wiki {
            cfg.do_docs = Confirm::new()
                .with_prompt("Launch docs viewer after wiki?")
                .default(false)
                .interact()?;
        }
        println!();

        // Step 6: LLM provider
        let llm_options = &["None (no AI enrichment)", "DeepSeek", "Gemini", "Anthropic", "OpenAI"];
        let llm_choice = Select::new()
            .with_prompt("LLM provider for wiki enrichment (optional)")
            .items(llm_options)
            .default(0)
            .interact()?;
        cfg.llm = match llm_choice {
            1 => LlmChoice::DeepSeek,
            2 => LlmChoice::Gemini,
            3 => LlmChoice::Anthropic,
            4 => LlmChoice::OpenAI,
            _ => LlmChoice::None,
        };
        println!();

        // Step 7: API key (if LLM selected)
        let llm_key_line = if cfg.llm != LlmChoice::None {
            let key_name = cfg.llm.env_var().unwrap_or("API_KEY");
            let key: String = Password::new()
                .with_prompt(format!("{} (input hidden)", key_name))
                .interact()?;
            if key.is_empty() {
                println!("  ⚠ no key entered — you can add it to .env later.");
                Some(format!("{}=", key_name))
            } else {
                Some(format!("{}={}", key_name, key))
            }
        } else {
            None
        };
        println!();

        // Step 8: Show summary
        println!("══ Configuration Summary ══");
        println!("  Workspace:      {}", cfg.workspace.display());
        println!("  Repo path:      {}", repo_canonical.display());
        println!("  Repo name:      {}", repo_name);
        println!(
            "  Index mode:     {}",
            match &cfg.index_mode {
                IndexMode::AnalyzeAll => "analyze all".to_string(),
                IndexMode::ScanOnly => "scan only".to_string(),
                IndexMode::Modules(m) => format!("modules: {}", m.join(", ")),
            }
        );
        println!("  Discover:       {}", if cfg.do_discover { "yes" } else { "no" });
        println!("  Embed:          {}", if cfg.do_embed { "yes" } else { "no" });
        println!("  Wiki:           {}", if cfg.do_wiki { "yes" } else { "no" });
        println!("  Docs viewer:    {}", if cfg.do_docs { "yes" } else { "no" });
        println!(
            "  LLM provider:   {}",
            match cfg.llm {
                LlmChoice::None => "none".to_string(),
                other => format!("{:?}", other),
            }
        );
        println!();

        if !Confirm::new()
            .with_prompt("Write .env and run preflight checks?")
            .default(true)
            .interact()?
        {
            println!("Cancelled. No files written.");
            return Ok(());
        }

        // Step 9: Preflight checks & write .env
        run_preflight_checks(&cfg.workspace, &repo_canonical, false)?;

        let content = start_env::render_env(&repo_canonical, &repo_name, llm_key_line.as_deref());
        start_env::write_env_file(&cfg.workspace, &content, false)?;
        println!("  ✓ .env written to {}/.env", cfg.workspace.display());

        // Step 10: Command plan & execution
        let plan = build_command_plan(&cfg);
        println!("\nCommand plan:");
        for cmd in &plan {
            let tag = if cmd.optional { "optional" } else { "required" };
            println!("  [{}] {}: {}", tag, cmd.label, cmd.command);
        }
        println!();

        let runner = RealCommandRunner;
        execute_command_plan(&plan, &runner, false, false)?;

        Ok(())
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
}
