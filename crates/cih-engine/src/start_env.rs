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
/// - `CIH_PG_URL=<host postgres url>` (for native dev; compose services override)
/// - Optional LLM key line (if `llm_key_line` is Some)
#[allow(dead_code)]
pub fn render_env(repo_path: &Path, repo_name: &str, llm_key_line: Option<&str>) -> String {
    let mut content = String::new();
    content.push_str("# CIH Interactive Start configuration\n");
    content.push_str(&format!("REPO_PATH={}\n", repo_path.display()));
    content.push_str(&format!("REPO_NAME={}\n", repo_name));
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub fn merge_env_values(
    existing_lines: &[String],
    repo_path: &Path,
    repo_name: &str,
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
            } else if key == "CIH_PG_URL" {
                // Preserve the existing value — the user may have customized the URL.
                found_pg_url = true;
                result.push(line.clone());
                continue;
            } else if LLM_KEYS.contains(&key) {
                // If we have a new llm_key_line, replace it; otherwise preserve the existing value
                if llm_key_line.is_some() {
                    found_llm_key = true;
                    result.push(llm_key_line.unwrap().trim_end().to_string());
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
    if !found_pg_url {
        result.push(format!("CIH_PG_URL={}", DEFAULT_PG_URL));
    }

    // Append optional LLM key line if it wasn't found in existing
    if llm_key_line.is_some() && !found_llm_key {
        result.push(llm_key_line.unwrap().trim_end().to_string());
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
#[allow(dead_code)]
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

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn temp_dir(suffix: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("cih-start-env-test-{}", suffix));
        let _ = fs::remove_dir_all(&path); // Clean up if exists
        fs::create_dir_all(&path).expect("failed to create temp dir");
        path
    }

    // ── render_env tests ────

    #[test]
    fn start_render_env_contains_required_keys() {
        let repo = Path::new("/home/user/myrepo");
        let output = render_env(repo, "myrepo", None);

        assert!(output.contains("# CIH Interactive Start configuration"));
        assert!(output.contains("REPO_PATH=/home/user/myrepo"));
        assert!(output.contains("REPO_NAME=myrepo"));
    }

    #[test]
    fn start_render_env_no_llm_has_no_key_line() {
        let repo = Path::new("/home/user/myrepo");
        let output = render_env(repo, "myrepo", None);

        assert!(!output.contains("DEEPSEEK_API_KEY"));
        assert!(!output.contains("GEMINI_API_KEY"));
        assert!(!output.contains("ANTHROPIC_API_KEY"));
        assert!(!output.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn start_render_env_with_llm_includes_key() {
        let repo = Path::new("/home/user/myrepo");
        let output = render_env(repo, "myrepo", Some("DEEPSEEK_API_KEY=sk-test-123"));

        assert!(output.contains("DEEPSEEK_API_KEY=sk-test-123"));
    }

    // ── load_env_file tests ────

    #[test]
    fn start_load_env_returns_empty_for_missing() {
        let temp = temp_dir("load-missing");
        let path = temp.join(".env");

        let result = load_env_file(&path).expect("load_env_file failed");
        assert_eq!(result, Vec::<String>::new());
    }

    #[test]
    fn start_load_env_returns_lines() {
        let temp = temp_dir("load-lines");
        let path = temp.join(".env");

        fs::write(&path, "REPO_PATH=/repo\nREPO_NAME=test\n").expect("write failed");

        let result = load_env_file(&path).expect("load_env_file failed");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "REPO_PATH=/repo");
        assert_eq!(result[1], "REPO_NAME=test");
    }

    // ── merge_env_values tests ────

    #[test]
    fn start_merge_preserves_comments() {
        let existing = vec!["# my comment".to_string(), "REPO_PATH=/old".to_string()];
        let repo = Path::new("/new");

        let output = merge_env_values(&existing, repo, "test", None);

        assert!(output.contains("# my comment"));
        assert!(output.contains("REPO_PATH=/new"));
    }

    #[test]
    fn start_merge_preserves_blank_lines() {
        let existing = vec!["".to_string(), "REPO_PATH=/old".to_string(), "".to_string()];
        let repo = Path::new("/new");

        let output = merge_env_values(&existing, repo, "test", None);

        let lines: Vec<&str> = output.lines().collect();
        // Should have blank lines preserved
        assert!(lines.iter().any(|l| l.is_empty()));
    }

    #[test]
    fn start_merge_preserves_unknown_keys() {
        let existing = vec!["FOO=bar".to_string(), "REPO_PATH=/old".to_string()];
        let repo = Path::new("/new");

        let output = merge_env_values(&existing, repo, "test", None);

        assert!(output.contains("FOO=bar"));
        assert!(output.contains("REPO_PATH=/new"));
    }

    #[test]
    fn start_merge_updates_existing_repo_path() {
        let existing = vec!["REPO_PATH=/old".to_string()];
        let repo = Path::new("/new");

        let output = merge_env_values(&existing, repo, "test", None);

        assert!(!output.contains("REPO_PATH=/old"));
        assert!(output.contains("REPO_PATH=/new"));
    }

    #[test]
    fn start_merge_appends_missing_keys() {
        let existing = vec!["# comment".to_string()];
        let repo = Path::new("/new");

        let output = merge_env_values(&existing, repo, "myrepo", None);

        assert!(output.contains("REPO_PATH=/new"));
        assert!(output.contains("REPO_NAME=myrepo"));
    }

    #[test]
    fn start_merge_llm_key_replaces_existing() {
        let existing = vec![
            "DEEPSEEK_API_KEY=sk-old".to_string(),
            "REPO_PATH=/old".to_string(),
        ];
        let repo = Path::new("/new");

        let output = merge_env_values(
            &existing,
            repo,
            "test",
            Some("DEEPSEEK_API_KEY=sk-test-123"),
        );

        assert!(!output.contains("DEEPSEEK_API_KEY=sk-old"));
        assert!(output.contains("DEEPSEEK_API_KEY=sk-test-123"));
    }

    #[test]
    fn start_merge_llm_key_appends_if_missing() {
        let existing = vec!["REPO_PATH=/old".to_string()];
        let repo = Path::new("/new");

        let output = merge_env_values(&existing, repo, "test", Some("GEMINI_API_KEY=sk-test-456"));

        assert!(output.contains("GEMINI_API_KEY=sk-test-456"));
    }

    #[test]
    fn start_merge_recognized_llm_keys() {
        // Test all recognized LLM keys
        for key_name in &[
            "DEEPSEEK_API_KEY",
            "GEMINI_API_KEY",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
        ] {
            let existing = vec![format!("{}=sk-old", key_name)];
            let repo = Path::new("/new");
            let new_line = format!("{}=sk-test-123", key_name);

            let output = merge_env_values(&existing, repo, "test", Some(&new_line));

            assert!(
                output.contains(&format!("{}=sk-test-123", key_name)),
                "failed for {}",
                key_name
            );
        }
    }

    // ── write_env_file tests ────

    #[test]
    fn start_write_new_env_creates_file() {
        let temp = temp_dir("write-new");
        let content = "REPO_PATH=/test\nREPO_NAME=test\n";

        let result = write_env_file(&temp, content, false).expect("write_env_file failed");

        let path = temp.join(".env");
        assert!(path.exists(), ".env file was not created");
        let written = fs::read_to_string(&path).expect("failed to read .env");
        assert_eq!(written, content);
        assert!(result.ends_with(".env"));
    }

    #[test]
    fn start_write_backup_created() {
        let temp = temp_dir("write-backup");
        let path = temp.join(".env");

        // Create an existing .env
        fs::write(&path, "REPO_PATH=/old\n").expect("initial write failed");

        let content = "REPO_PATH=/new\nREPO_NAME=test\n";
        let result = write_env_file(&temp, content, false).expect("write_env_file failed");

        // Check that .env was updated
        let written = fs::read_to_string(&path).expect("failed to read .env");
        assert_eq!(written, content);

        // Check that backup exists (name includes timestamp)
        let backup_files: Vec<_> = fs::read_dir(&temp)
            .expect("failed to read temp dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".env.cih-backup-")
            })
            .collect();

        assert_eq!(backup_files.len(), 1, "backup file was not created");
        let backup_content =
            fs::read_to_string(backup_files[0].path()).expect("failed to read backup");
        assert_eq!(backup_content, "REPO_PATH=/old\n");
        assert!(result.ends_with(".env"));
    }

    #[test]
    fn start_dry_run_writes_no_env_file() {
        let temp = temp_dir("dry-run");
        let content = "REPO_PATH=/test\nREPO_NAME=test\n";

        let result = write_env_file(&temp, content, true).expect("write_env_file failed");

        // .env should NOT exist
        let path = temp.join(".env");
        assert!(!path.exists(), ".env file was created in dry_run mode");

        // But the result should still return the path
        assert!(result.ends_with(".env"));
    }

    #[test]
    fn start_dry_run_no_backup() {
        let temp = temp_dir("dry-run-no-backup");
        let path = temp.join(".env");

        // Create an existing .env
        fs::write(&path, "REPO_PATH=/old\n").expect("initial write failed");

        let content = "REPO_PATH=/new\nREPO_NAME=test\n";
        write_env_file(&temp, content, true).expect("write_env_file failed");

        // Original .env should be unchanged
        let written = fs::read_to_string(&path).expect("failed to read .env");
        assert_eq!(written, "REPO_PATH=/old\n");

        // No backup file should exist
        let backup_files: Vec<_> = fs::read_dir(&temp)
            .expect("failed to read temp dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".env.cih-backup-")
            })
            .collect();

        assert_eq!(
            backup_files.len(),
            0,
            "backup file was created in dry_run mode"
        );
    }
}
