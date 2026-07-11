use rmcp::{model::CallToolResult, ErrorData as McpError};

use crate::args::{GrepFilesArgs, ReadFileArgs};
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
    let canon_path = full_path.canonicalize().map_err(|e| {
        McpError::invalid_params(format!("cannot resolve '{}': {e}", args.path), None)
    })?;
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

/// Skip files larger than this during a grep walk — keeps stray artifacts
/// (fat jars, dumps) from being pulled into memory.
const GREP_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Cap on returned match text — one minified single-line file must not flood
/// the agent's context through a single match.
const GREP_MAX_TEXT_BYTES: usize = 500;
/// Default / hard-cap on the number of returned matches.
const GREP_DEFAULT_LIMIT: usize = 200;
const GREP_MAX_LIMIT: usize = 1000;

/// Build/vendor directories to skip even when no gitignore applies (sources
/// copied without `.git` — e.g. into a Docker volume — get no gitignore
/// filtering from the `ignore` crate).
const GREP_SKIP_DIRS: &[&str] = &["target", "node_modules", "build", "dist", ".git"];

#[derive(serde::Serialize)]
pub struct GrepMatch {
    pub file: String,
    pub line: u32,
    pub text: String,
}

pub async fn grep_files(graph_key: &str, args: GrepFilesArgs) -> Result<CallToolResult, McpError> {
    // Validate the cheap, registry-free inputs first.
    let regex = compile_pattern(&args.pattern)?;
    let glob = compile_glob(&args.glob)?;

    let repo_root = find_repo_path(
        if args.repo.is_empty() {
            None
        } else {
            Some(args.repo.as_str())
        },
        graph_key,
    )
    .map_err(|e| McpError::invalid_params(e, None))?;

    let limit = if args.limit == 0 {
        GREP_DEFAULT_LIMIT
    } else {
        args.limit
    }
    .min(GREP_MAX_LIMIT);

    // The walk is synchronous filesystem I/O over the whole repo — keep it off
    // the async workers.
    let root = std::path::PathBuf::from(&repo_root);
    let (matches, truncated) =
        tokio::task::spawn_blocking(move || grep_dir(&root, &regex, glob.as_ref(), limit))
            .await
            .map_err(|e| McpError::internal_error(format!("grep task failed: {e}"), None))?;

    json_result(&serde_json::json!({
        "pattern": args.pattern,
        "glob": args.glob,
        "matches_returned": matches.len(),
        "truncated": truncated,
        "matches": matches,
    }))
}

fn compile_pattern(pattern: &str) -> Result<regex::Regex, McpError> {
    regex::Regex::new(pattern)
        .map_err(|e| McpError::invalid_params(format!("invalid regex pattern: {e}"), None))
}

fn compile_glob(glob: &str) -> Result<Option<globset::GlobSet>, McpError> {
    if glob.is_empty() {
        return Ok(None);
    }
    let mut builder = globset::GlobSetBuilder::new();
    builder.add(
        globset::Glob::new(glob)
            .map_err(|e| McpError::invalid_params(format!("invalid glob: {e}"), None))?,
    );
    builder
        .build()
        .map_err(|e| McpError::invalid_params(format!("invalid glob: {e}"), None))
        .map(Some)
}

/// Gitignore-aware regex scan under `root`. Separated from repo resolution so
/// it is unit-testable without the registry. Returns matches in walk order and
/// whether the `limit` cut the scan short.
fn grep_dir(
    root: &std::path::Path,
    regex: &regex::Regex,
    glob: Option<&globset::GlobSet>,
    limit: usize,
) -> (Vec<GrepMatch>, bool) {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .add_custom_ignore_filename(".cihignore")
        .filter_entry(|entry| {
            if entry.depth() > 0 && entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy();
                return !GREP_SKIP_DIRS.contains(&name.as_ref());
            }
            true
        });

    let mut matches = Vec::new();
    let mut truncated = false;
    'files: for entry in builder.build().flatten() {
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        // The walker does not follow symlinks, so a symlinked file is the only
        // way a read could escape the repo root — skip them.
        if entry.path_is_symlink() {
            continue;
        }
        let rel = match entry.path().strip_prefix(root) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        if let Some(glob) = glob {
            if !glob.is_match(rel) {
                continue;
            }
        }
        match entry.metadata() {
            Ok(md) if md.len() <= GREP_MAX_FILE_BYTES => {}
            _ => continue,
        }
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes.contains(&0) {
            continue; // binary heuristic
        }
        let content = String::from_utf8_lossy(&bytes);
        for (i, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                matches.push(GrepMatch {
                    file: rel.to_string_lossy().into_owned(),
                    line: i as u32 + 1,
                    text: cap_text(line, GREP_MAX_TEXT_BYTES),
                });
                if matches.len() >= limit {
                    truncated = true;
                    break 'files;
                }
            }
        }
    }
    (matches, truncated)
}

/// Truncate to at most `max` bytes on a char boundary, marking the cut.
fn cap_text(line: &str, max: usize) -> String {
    if line.len() <= max {
        return line.to_string();
    }
    let mut end = max;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
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

    /// Fresh temp dir for a grep test; recreated on every run.
    fn grep_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("cih-grepfiles-test-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_under(root: &std::path::Path, rel: &str, contents: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    fn re(pattern: &str) -> regex::Regex {
        regex::Regex::new(pattern).unwrap()
    }

    #[test]
    fn grep_finds_match_with_file_line_text() {
        let root = grep_root("basic");
        write_under(
            &root,
            "src/Foo.java",
            b"class Foo {\n  // TODO fix this\n}\n",
        );
        let (matches, truncated) = grep_dir(&root, &re("TODO"), None, 100);
        assert!(!truncated);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file, "src/Foo.java");
        assert_eq!(matches[0].line, 2);
        assert_eq!(matches[0].text, "  // TODO fix this");
    }

    #[test]
    fn grep_glob_filters_files() {
        let root = grep_root("glob");
        write_under(&root, "a/Foo.java", b"// TODO java\n");
        write_under(&root, "b/bar.rs", b"// TODO rust\n");
        let glob = compile_glob("**/*.java").unwrap().unwrap();
        let (matches, _) = grep_dir(&root, &re("TODO"), Some(&glob), 100);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file, "a/Foo.java");
    }

    #[test]
    fn grep_limit_truncates() {
        let root = grep_root("limit");
        let body: String = (1..=10).map(|i| format!("TODO {i}\n")).collect();
        write_under(&root, "many.txt", body.as_bytes());
        let (matches, truncated) = grep_dir(&root, &re("TODO"), None, 3);
        assert!(truncated);
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn grep_skips_binary_files() {
        let root = grep_root("binary");
        write_under(&root, "blob.bin", b"TODO\0TODO\n");
        let (matches, _) = grep_dir(&root, &re("TODO"), None, 100);
        assert!(matches.is_empty());
    }

    #[test]
    fn grep_caps_long_match_text() {
        let root = grep_root("longline");
        let line = format!("TODO {}", "x".repeat(2000));
        write_under(&root, "minified.js", line.as_bytes());
        let (matches, _) = grep_dir(&root, &re("TODO"), None, 100);
        assert_eq!(matches.len(), 1);
        assert!(matches[0].text.len() <= GREP_MAX_TEXT_BYTES + '…'.len_utf8());
        assert!(matches[0].text.ends_with('…'));
    }

    #[test]
    fn grep_skips_build_dirs() {
        let root = grep_root("skipdirs");
        write_under(&root, "node_modules/dep/x.js", b"// TODO vendored\n");
        write_under(&root, "target/debug/x.rs", b"// TODO generated\n");
        write_under(&root, "src/x.rs", b"// TODO real\n");
        let (matches, _) = grep_dir(&root, &re("TODO"), None, 100);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file, "src/x.rs");
    }

    #[test]
    fn invalid_pattern_is_rejected() {
        let err = compile_pattern("[unclosed").unwrap_err();
        assert!(
            err.message.contains("invalid regex"),
            "unexpected: {}",
            err.message
        );
    }
}
