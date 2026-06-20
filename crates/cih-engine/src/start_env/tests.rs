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
