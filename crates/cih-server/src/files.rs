use rmcp::{model::CallToolResult, ErrorData as McpError};

use crate::args::ReadFileArgs;
use crate::symbol::find_repo_path;
use crate::utils::json_result;

pub async fn read_file(graph_key: &str, args: ReadFileArgs) -> Result<CallToolResult, McpError> {
    let repo_root = find_repo_path(args.repo.as_deref(), graph_key)
        .map_err(|e| McpError::invalid_params(e, None))?;

    let clean = std::path::Path::new(&args.path);
    if clean.components().any(|c| c == std::path::Component::ParentDir) {
        return Err(McpError::invalid_params(
            "path must not contain '..' components",
            None,
        ));
    }

    let full_path = std::path::Path::new(&repo_root).join(clean);
    let content = std::fs::read_to_string(&full_path).map_err(|e| {
        McpError::invalid_params(format!("cannot read '{}': {e}", args.path), None)
    })?;

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len() as u32;
    let start = args.start_line.unwrap_or(1).max(1);
    let end = args.end_line.unwrap_or(total).min(total);

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

    json_result(&serde_json::json!({
        "path": args.path,
        "total_lines": total,
        "start_line": start,
        "end_line": end.min(total),
        "content": slice,
    }))
}
