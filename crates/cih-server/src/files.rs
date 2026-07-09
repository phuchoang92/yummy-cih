use rmcp::{model::CallToolResult, ErrorData as McpError};

use crate::args::ReadFileArgs;
use crate::symbol::find_repo_path;
use crate::utils::json_result;

/// Caps applied by `read_file` to keep large files out of the agent's context.
#[derive(Clone, Copy)]
pub struct ReadFileLimits {
    /// Reject files larger than this many bytes.
    pub max_bytes: u64,
    /// Cap on returned lines when the caller gives no explicit range.
    pub max_lines: usize,
}

pub async fn read_file(
    graph_key: &str,
    limits: ReadFileLimits,
    args: ReadFileArgs,
) -> Result<CallToolResult, McpError> {
    let repo_root = find_repo_path(
        if args.repo.is_empty() {
            None
        } else {
            Some(args.repo.as_str())
        },
        graph_key,
    )
    .map_err(|e| McpError::invalid_params(e, None))?;

    let clean = std::path::Path::new(&args.path);
    if clean
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(McpError::invalid_params(
            "path must not contain '..' components",
            None,
        ));
    }

    let full_path = std::path::Path::new(&repo_root).join(clean);

    // Canonicalize both paths to resolve symlinks before the containment check.
    // This prevents a symlink inside the repo from pointing outside the root.
    let canon_root = std::path::Path::new(&repo_root)
        .canonicalize()
        .map_err(|e| McpError::invalid_params(format!("cannot resolve repo root: {e}"), None))?;
    let canon_path = full_path
        .canonicalize()
        .map_err(|e| McpError::invalid_params(format!("cannot resolve '{}': {e}", args.path), None))?;
    if !canon_path.starts_with(&canon_root) {
        return Err(McpError::invalid_params("path escapes repo root", None));
    }

    let value = read_sliced(
        &canon_path,
        &args.path,
        limits,
        args.start_line,
        args.end_line,
    )?;
    json_result(&value)
}

/// Size-check, read, and line-slice a resolved file path. Separated from repo
/// resolution so it is unit-testable without the registry.
fn read_sliced(
    full_path: &std::path::Path,
    path_label: &str,
    limits: ReadFileLimits,
    start_line: u32,
    end_line: u32,
) -> Result<serde_json::Value, McpError> {
    // Reject oversized files before reading them into memory.
    let file_size = std::fs::metadata(full_path)
        .map_err(|e| McpError::invalid_params(format!("cannot stat '{path_label}': {e}"), None))?
        .len();
    if file_size > limits.max_bytes {
        return Err(McpError::invalid_params(
            format!(
                "file '{path_label}' is {file_size} bytes, over the {}-byte read limit. \
                 Pass start_line/end_line to read a section, or raise CIH_READ_FILE_MAX_BYTES.",
                limits.max_bytes
            ),
            None,
        ));
    }

    let content = std::fs::read_to_string(full_path)
        .map_err(|e| McpError::invalid_params(format!("cannot read '{path_label}': {e}"), None))?;

    let explicit_range = start_line != 0 || end_line != 0;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len() as u32;
    let start = (if start_line == 0 { 1 } else { start_line }).max(1);
    let mut end = (if end_line == 0 { total } else { end_line }).min(total);

    // With no explicit range, cap the number of returned lines so a very long
    // file doesn't flood the agent's context. Tell the caller when we truncate.
    let mut truncated = false;
    if !explicit_range && end >= start && (end - start + 1) as usize > limits.max_lines {
        end = start + limits.max_lines as u32 - 1;
        truncated = true;
    }

    let slice = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            let ln = *i as u32 + 1;
            ln >= start && ln <= end
        })
        .map(|(i, line)| format!("{:>4} {}", i as u32 + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(serde_json::json!({
        "path": path_label,
        "total_lines": total,
        "start_line": start,
        "end_line": end.min(total),
        "truncated": truncated,
        "note": if truncated {
            Some(format!(
                "output capped at {} lines; pass start_line/end_line to read further",
                limits.max_lines
            ))
        } else {
            None
        },
        "content": slice,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_write(name: &str, contents: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("cih-readfile-test-{name}"));
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn oversized_file_is_rejected() {
        let p = tmp_write("big", &"x".repeat(1000));
        let limits = ReadFileLimits {
            max_bytes: 100,
            max_lines: 5000,
        };
        let err = read_sliced(&p, "big.txt", limits, 0, 0).unwrap_err();
        assert!(
            err.message.contains("over the"),
            "unexpected: {}",
            err.message
        );
    }

    #[test]
    fn unranged_read_truncates_at_line_cap() {
        let body: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let p = tmp_write("lines", &body);
        let limits = ReadFileLimits {
            max_bytes: 10 * 1024 * 1024,
            max_lines: 5,
        };
        let v = read_sliced(&p, "lines.txt", limits, 0, 0).unwrap();
        assert_eq!(v["truncated"], serde_json::json!(true));
        assert_eq!(v["total_lines"], serde_json::json!(20));
        assert_eq!(v["end_line"], serde_json::json!(5));
        assert!(v["content"].as_str().unwrap().contains("line5"));
        assert!(!v["content"].as_str().unwrap().contains("line6"));
    }

    #[test]
    fn explicit_range_is_not_capped() {
        let body: String = (1..=20).map(|i| format!("line{i}\n")).collect();
        let p = tmp_write("range", &body);
        let limits = ReadFileLimits {
            max_bytes: 10 * 1024 * 1024,
            max_lines: 5,
        };
        let v = read_sliced(&p, "range.txt", limits, 1, 20).unwrap();
        assert_eq!(v["truncated"], serde_json::json!(false));
        assert_eq!(v["end_line"], serde_json::json!(20));
    }

    #[test]
    fn small_file_reads_whole() {
        let p = tmp_write("small", "a\nb\nc\n");
        let limits = ReadFileLimits {
            max_bytes: 10 * 1024 * 1024,
            max_lines: 5000,
        };
        let v = read_sliced(&p, "small.txt", limits, 0, 0).unwrap();
        assert_eq!(v["truncated"], serde_json::json!(false));
        assert_eq!(v["total_lines"], serde_json::json!(3));
    }
}
