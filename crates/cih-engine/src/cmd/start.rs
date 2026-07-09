use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use dialoguer::{Confirm, Input, Password, Select};

use crate::cmd::start_env;

// ── Timeline UI ────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Palette {
    has_color: bool,
}

fn palette() -> &'static Palette {
    static PAL: OnceLock<Palette> = OnceLock::new();
    PAL.get_or_init(|| {
        // console::colors_enabled() honors NO_COLOR and non-TTY output.
        Palette {
            has_color: console::colors_enabled(),
        }
    })
}

fn paint(active: bool, code: &str, text: &str) -> String {
    if active {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn paint_dim(active: bool, text: &str) -> String {
    paint(active, "2", text)
}

fn step_begin(name: &str) {
    let pal = palette();
    let dot = paint(pal.has_color, "36", "●");
    let label = paint(pal.has_color, "1", name);
    println!("\n {dot}  {label}");
}

fn step_blank() {
    let pal = palette();
    let bar = paint_dim(pal.has_color, "│");
    println!(" {bar}");
}

fn step_line(text: &str) {
    let pal = palette();
    let bar = paint_dim(pal.has_color, "│");
    println!(" {bar}  {text}");
}

fn step_warn(text: &str) {
    let pal = palette();
    let bar = paint_dim(pal.has_color, "│");
    let warn = paint(pal.has_color, "33", "⚠");
    let label = paint(pal.has_color, "1", &format!("{warn} {text}"));
    println!(" {bar}  {label}");
}

fn step_ok(name: &str) {
    let pal = palette();
    let dot = paint(pal.has_color, "32", "✓");
    let label = paint(pal.has_color, "1", name);
    println!("\n {dot}  {label}");
}

fn step_fail(text: &str) {
    let pal = palette();
    let dot = paint(pal.has_color, "31", "✗");
    let label = paint(pal.has_color, "1", text);
    eprintln!("\n {dot}  {label}");
}

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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IndexMode {
    /// Only scan — no analyze step.
    ScanOnly,
    /// Analyze all modules.
    #[default]
    AnalyzeAll,
    /// Analyze specific modules by name.
    Modules(Vec<String>),
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
    /// Postgres password. Required in non-interactive mode (or read from
    /// the `POSTGRES_PASSWORD` env var). Prompted (hidden) in interactive mode.
    pub postgres_password: Option<String>,
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
            postgres_password: None,
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
    step_begin("Preflight checks");

    let compose_file = workspace.join("docker-compose.yml");
    if !compose_file.exists() {
        step_fail(&format!(
            "docker-compose.yml not found in workspace: {}",
            workspace.display()
        ));
        anyhow::bail!(
            "docker-compose.yml not found in workspace: {}",
            workspace.display()
        );
    }
    step_line(&format!(
        "docker-compose.yml found: {}",
        compose_file.display()
    ));

    let docker_ok = std::process::Command::new("docker")
        .args(["compose", "version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !docker_ok {
        if dry_run {
            step_warn("docker compose not available (dry-run continues)");
        } else {
            step_warn("docker compose not available — commands may fail");
        }
    } else {
        step_line("docker compose available");
    }

    let canonical = validate_repo_path(repo_path)?;
    step_line(&format!("repo path exists: {}", canonical.display()));

    let has_java = dir_contains_java(&canonical);
    if !has_java {
        step_warn(&format!(
            "no .java files found under {} (indexing may produce no artifacts)",
            canonical.display()
        ));
    } else {
        step_line(".java files detected");
    }

    step_ok("Preflight checks");
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
    step_begin("Executing command plan");

    for cmd in plan {
        step_blank();
        let tag = if cmd.optional {
            "(optional)"
        } else {
            "(required)"
        };
        step_line(&format!("[{}] {}", cmd.label, cmd.command));
        step_line(tag);

        if dry_run {
            step_line("[DRY RUN] would execute, skipping");
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
            step_line(&format!("Skipped. Copy/paste: {}", cmd.command));
            continue;
        }

        match runner.run(&cmd.label, &cmd.command) {
            Ok(()) => step_line("done"),
            Err(e) => {
                step_fail(&format!("[{}] failed: {}", cmd.label, e));
                if !cmd.optional {
                    anyhow::bail!("required command [{}] failed — aborting", cmd.label);
                }
                step_line("(optional — continuing)");
            }
        }
    }

    step_ok("Executing command plan");
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

        let pg_password = match cfg.postgres_password.clone() {
            Some(p) if !p.is_empty() => p,
            _ => match std::env::var("POSTGRES_PASSWORD") {
                Ok(p) if !p.is_empty() => p,
                _ => bail!(
                    "Error: POSTGRES_PASSWORD is required in --non-interactive mode \
                     (pass --postgres-password or export POSTGRES_PASSWORD)"
                ),
            },
        };

        run_preflight_checks(&cfg.workspace, &repo_canonical, cfg.dry_run)?;

        let plan = build_command_plan(&cfg);

        let llm_key_line = cfg.llm.env_var().map(|key| format!("{}=", key));
        let content = start_env::render_env(
            &repo_canonical,
            &repo_name,
            &pg_password,
            llm_key_line.as_deref(),
        );

        start_env::write_env_file(&cfg.workspace, &content, cfg.dry_run)?;

        step_begin("Command plan");
        for cmd in &plan {
            let tag = if cmd.optional { "optional" } else { "required" };
            step_line(&format!("[{}] {}: {}", tag, cmd.label, cmd.command));
        }
        step_blank();

        let runner = RealCommandRunner;
        execute_command_plan(&plan, &runner, cfg.dry_run, cfg.non_interactive)?;

        Ok(())
    } else {
        // ── Interactive mode ──
        println!();
        let title = paint(
            palette().has_color,
            "1",
            "══ CIH Interactive Setup Wizard ══",
        );
        println!("{title}");

        // Step 1: Workspace
        step_begin("CIH workspace directory");
        let workspace_default = cfg.workspace.display().to_string();
        let workspace: String = Input::new()
            .with_prompt("CIH workspace directory (must contain docker-compose.yml)")
            .default(workspace_default)
            .interact_text()?;
        cfg.workspace = PathBuf::from(&workspace);
        step_line(&format!("workspace: {}", cfg.workspace.display()));

        // Step 2: Repo path (loop until valid)
        step_begin("Java/Spring repository path");
        let repo_canonical = loop {
            let repo_input: String = Input::new()
                .with_prompt("Java/Spring repository path (absolute)")
                .interact_text()?;
            let repo = PathBuf::from(&repo_input);
            match validate_repo_path(&repo) {
                Ok(p) => {
                    step_line(&format!("found: {}", p.display()));
                    break p;
                }
                Err(e) => step_fail(&format!("{}. Try again.", e)),
            }
        };
        cfg.repo = Some(repo_canonical.clone());

        // Step 3: Repo name
        step_begin("Repository name");
        let default_name = default_repo_name(&repo_canonical);
        let repo_name: String = Input::new()
            .with_prompt("Repository name (URL prefix for docs viewer)")
            .default(default_name)
            .interact_text()?;
        cfg.repo_name = Some(repo_name.clone());
        step_line(&format!("repo name: {}", repo_name));

        // Step 4: Index mode
        step_begin("Indexing scope");
        let index_options = &[
            "analyze all modules",
            "scan only (no analyze)",
            "specific modules",
        ];
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
                    step_warn("no modules entered, defaulting to analyze-all");
                    IndexMode::AnalyzeAll
                } else {
                    IndexMode::Modules(modules)
                }
            }
            _ => IndexMode::AnalyzeAll,
        };
        match &cfg.index_mode {
            IndexMode::AnalyzeAll => step_line("mode: analyze all"),
            IndexMode::ScanOnly => step_line("mode: scan only"),
            IndexMode::Modules(m) => step_line(&format!("mode: modules: {}", m.join(", "))),
        }

        // Step 5: Optional steps
        step_begin("Optional steps");
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
        step_line(&format!(
            "discover={}, embed={}, wiki={}, docs={}",
            bool_str(cfg.do_discover),
            bool_str(cfg.do_embed),
            bool_str(cfg.do_wiki),
            bool_str(cfg.do_docs),
        ));

        // Step 6: LLM provider
        step_begin("LLM provider");
        let llm_options = &[
            "None (no AI enrichment)",
            "DeepSeek",
            "Gemini",
            "Anthropic",
            "OpenAI",
        ];
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
        match cfg.llm {
            LlmChoice::None => step_line("provider: none"),
            other => step_line(&format!("provider: {:?}", other)),
        }

        // Step 7: API key (if LLM selected)
        let llm_key_line = if cfg.llm != LlmChoice::None {
            step_begin("LLM API key");
            let key_name = cfg.llm.env_var().unwrap_or("API_KEY");
            let key: String = Password::new()
                .with_prompt(format!("{} (input hidden)", key_name))
                .interact()?;
            if key.is_empty() {
                step_warn("no key entered — you can add it to .env later.");
                Some(format!("{}=", key_name))
            } else {
                step_line("key accepted");
                Some(format!("{}={}", key_name, key))
            }
        } else {
            None
        };

        // Step 7b: Postgres password (required, hidden, ≤3 attempts — contract §3.3)
        let pg_password = prompt_required_password(
            "Postgres password (POSTGRES_PASSWORD, hidden)",
            "POSTGRES_PASSWORD cannot be empty.",
        )?;

        // Step 8: Show summary
        step_begin("Configuration summary");
        step_line(&format!("  Workspace:      {}", cfg.workspace.display()));
        step_line(&format!("  Repo path:      {}", repo_canonical.display()));
        step_line(&format!("  Repo name:      {}", repo_name));
        step_line("  Postgres pwd:   ********");
        step_line(&format!(
            "  Index mode:     {}",
            match &cfg.index_mode {
                IndexMode::AnalyzeAll => "analyze all".to_string(),
                IndexMode::ScanOnly => "scan only".to_string(),
                IndexMode::Modules(m) => format!("modules: {}", m.join(", ")),
            }
        ));
        step_line(&format!("  Discover:       {}", bool_str(cfg.do_discover)));
        step_line(&format!("  Embed:          {}", bool_str(cfg.do_embed)));
        step_line(&format!("  Wiki:           {}", bool_str(cfg.do_wiki)));
        step_line(&format!("  Docs viewer:    {}", bool_str(cfg.do_docs)));
        step_line(&format!(
            "  LLM provider:   {}",
            match cfg.llm {
                LlmChoice::None => "none".to_string(),
                other => format!("{:?}", other),
            }
        ));

        if !Confirm::new()
            .with_prompt("Write .env and run preflight checks?")
            .default(true)
            .interact()?
        {
            step_warn("cancelled — no files written.");
            return Ok(());
        }

        // Step 9: Preflight checks & write .env
        run_preflight_checks(&cfg.workspace, &repo_canonical, false)?;

        let content = start_env::render_env(
            &repo_canonical,
            &repo_name,
            &pg_password,
            llm_key_line.as_deref(),
        );
        start_env::write_env_file(&cfg.workspace, &content, false)?;
        step_line(&format!(".env written to {}/.env", cfg.workspace.display()));

        // Step 10: Command plan & execution
        let plan = build_command_plan(&cfg);
        step_begin("Command plan");
        for cmd in &plan {
            let tag = if cmd.optional { "optional" } else { "required" };
            step_line(&format!("[{}] {}: {}", tag, cmd.label, cmd.command));
        }

        let runner = RealCommandRunner;
        execute_command_plan(&plan, &runner, false, false)?;

        step_ok("CIH Interactive Setup Wizard complete");
        Ok(())
    }
}

fn bool_str(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

// Hidden password prompt with non-empty validation and ≤3 attempts.
// `empty_error` is the exact contract-mandated message (setup.sh / setup.bat).
fn prompt_required_password(prompt: &str, empty_error: &str) -> Result<String> {
    let mut attempts = 0;
    loop {
        let pw: String = Password::new()
            .with_prompt(prompt)
            .interact()
            .unwrap_or_default();
        if !pw.is_empty() {
            step_line("password accepted");
            return Ok(pw);
        }
        attempts += 1;
        step_fail(&format!("ERROR: {empty_error}"));
        if attempts >= 3 {
            bail!("ERROR: {empty_error}");
        }
    }
}
