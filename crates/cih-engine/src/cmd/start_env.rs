use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

// ── Core API ────────────────────────────────────────────────────────────────

/// Host-side Postgres URL written to `.env` for native `cargo run` use.
/// Inside Docker, the `engine` and `cih-server` services override this with
/// the internal service hostname (`postgres:5432`) via their `environment:` blocks.
const DEFAULT_PG_URL: &str = "postgres://cih:cih@localhost:5433/cih";

/// Render a complete .env file with header and required + optional keys.
///
/// Returns full .env content with:
/// - Header: `# CIH Interactive Start configuration`
/// - `REPO_PATH=<canonical_path>`
/// - `REPO_NAME=<name>`
/// - `POSTGRES_PASSWORD=<password>` (required by docker-compose.yml:${POSTGRES_PASSWORD:?...})
/// - `CIH_PG_URL=<host postgres url>` (for native dev; compose services override)
/// - Optional LLM key line (if `llm_key_line` is Some)
pub fn render_env(
    repo_path: &Path,
    repo_name: &str,
    postgres_password: &str,
    llm_key_line: Option<&str>,
) -> String {
    let mut content = String::new();
    content.push_str("# CIH Interactive Start configuration\n");
    content.push_str(&format!("REPO_PATH={}\n", repo_path.display()));
    content.push_str(&format!("REPO_NAME={}\n", repo_name));
    content.push_str(&format!("POSTGRES_PASSWORD={}\n", postgres_password));
    content.push_str(&format!("CIH_PG_URL={}\n", DEFAULT_PG_URL));

    if let Some(key_line) = llm_key_line {
        content.push_str(key_line);
        if !key_line.ends_with('\n') {
            content.push('\n');
        }
    }

    content
}

/// Load raw lines from an existing .env file.
///
/// Returns Vec of raw line strings (including comments and blanks).
/// Returns Ok(vec![]) if the file does not exist.
pub fn load_env_file(path: &Path) -> Result<Vec<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(content.lines().map(|s| s.to_string()).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e).context(format!("failed to read .env file at {}", path.display())),
    }
}

/// Merge existing .env lines with new required keys, preserving unknown lines.
///
/// Walks `existing_lines` and:
/// - For `REPO_PATH=`, `REPO_NAME=`, or LLM key env vars, replaces with new value
/// - For `CIH_PG_URL=`, preserves the existing value (user may have customized it)
/// - For all other lines (comments, blanks, unknown keys), preserves as-is
/// - Appends any REQUIRED keys (REPO_PATH, REPO_NAME, CIH_PG_URL) not found in existing
/// - Appends optional LLM key line if it wasn't already replaced
///
/// LLM key env var names recognized: DEEPSEEK_API_KEY, GEMINI_API_KEY, ANTHROPIC_API_KEY, OPENAI_API_KEY
pub fn merge_env_values(
    existing_lines: &[String],
    repo_path: &Path,
    repo_name: &str,
    postgres_password: &str,
    llm_key_line: Option<&str>,
) -> String {
    const LLM_KEYS: &[&str] = &[
        "DEEPSEEK_API_KEY",
        "GEMINI_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
    ];

    let mut result = Vec::new();
    let mut found_repo_path = false;
    let mut found_repo_name = false;
    let mut found_pg_pass = false;
    let mut found_llm_key = false;
    let mut found_pg_url = false;

    // Walk existing lines and update known keys
    for line in existing_lines {
        let trimmed = line.trim();

        if let Some((key, _)) = parse_env_line(trimmed) {
            if key == "REPO_PATH" {
                result.push(format!("REPO_PATH={}", repo_path.display()));
                found_repo_path = true;
                continue;
            } else if key == "REPO_NAME" {
                result.push(format!("REPO_NAME={}", repo_name));
                found_repo_name = true;
                continue;
            } else if key == "POSTGRES_PASSWORD" {
                result.push(format!("POSTGRES_PASSWORD={}", postgres_password));
                found_pg_pass = true;
                continue;
            } else if key == "CIH_PG_URL" {
                // Preserve the existing value — the user may have customized the URL.
                found_pg_url = true;
                result.push(line.clone());
                continue;
            } else if LLM_KEYS.contains(&key) {
                // If we have a new llm_key_line, replace it; otherwise preserve the existing value
                if let Some(llm_key) = llm_key_line {
                    found_llm_key = true;
                    result.push(llm_key.trim_end().to_string());
                } else {
                    result.push(line.clone());
                }
                continue;
            }
        }

        // Preserve all other lines (comments, blanks, unknown keys)
        result.push(line.clone());
    }

    // Append missing REQUIRED keys
    if !found_repo_path {
        result.push(format!("REPO_PATH={}", repo_path.display()));
    }
    if !found_repo_name {
        result.push(format!("REPO_NAME={}", repo_name));
    }
    if !found_pg_pass {
        result.push(format!("POSTGRES_PASSWORD={}", postgres_password));
    }
    if !found_pg_url {
        result.push(format!("CIH_PG_URL={}", DEFAULT_PG_URL));
    }

    // Append optional LLM key line if it wasn't found in existing
    if !found_llm_key {
        if let Some(llm_key) = llm_key_line {
            result.push(llm_key.trim_end().to_string());
        }
    }

    result.join("\n") + "\n"
}

/// Write .env file to workspace, with optional dry-run and backup.
///
/// If `dry_run` is true:
/// - Prints to stdout: "=== Would write to <path> ==="
/// - Returns the path string without writing
///
/// If `.env` exists at workspace:
/// - Creates backup at `.env.cih-backup-<unix_timestamp>`
/// - Uses `SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()` for timestamp
///
/// Then writes `content` to `<workspace>/.env` and returns the path string.
pub fn write_env_file(workspace: &Path, content: &str, dry_run: bool) -> Result<String> {
    let env_path = workspace.join(".env");

    if dry_run {
        println!("=== Would write to {} ===", env_path.display());
        println!("{}", content);
        return Ok(env_path.to_string_lossy().to_string());
    }

    // Create backup if .env exists
    if env_path.exists() {
        let unix_ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .context("failed to get current time")?
            .as_secs();
        let backup_path = workspace.join(format!(".env.cih-backup-{}", unix_ts));
        fs::copy(&env_path, &backup_path).context(format!(
            "failed to create backup at {}",
            backup_path.display()
        ))?;
    }

    // Write .env file
    let mut file = fs::File::create(&env_path).context(format!(
        "failed to create .env file at {}",
        env_path.display()
    ))?;
    file.write_all(content.as_bytes())
        .context("failed to write to .env file")?;

    Ok(env_path.to_string_lossy().to_string())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Parse a line as `KEY=VALUE`.
/// Returns None if the line is empty, a comment, or not a valid KEY=VALUE pair.
fn parse_env_line(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();

    // Skip comments and blank lines
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    // Split on first '='
    let (key, rest) = trimmed.split_once('=')?;
    Some((key.trim(), rest))
}
