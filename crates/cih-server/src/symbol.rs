use std::sync::Arc;

use cih_core::{Node, NodeId};
use cih_graph_store::GraphStore;
use rmcp::ErrorData as McpError;
use serde::Serialize;

use crate::utils::to_mcp;

pub enum SymbolResolution {
    Id(NodeId),
    Ambiguous(Vec<Node>),
    NotFound,
}

#[derive(Serialize)]
pub struct AmbiguousCandidate {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub file: String,
}

#[derive(Serialize)]
pub struct AmbiguousResult {
    pub status: &'static str,
    pub candidates: Vec<AmbiguousCandidate>,
}

impl AmbiguousResult {
    pub fn from_nodes(nodes: Vec<Node>) -> Self {
        AmbiguousResult {
            status: "ambiguous",
            candidates: nodes
                .into_iter()
                .map(|n| AmbiguousCandidate {
                    id: n.id.to_string(),
                    kind: n.kind.label().to_string(),
                    name: n.name,
                    file: n.file,
                })
                .collect(),
        }
    }
}

/// Resolve a name to a NodeId: if it already contains `:` treat it as a
/// full NodeId; otherwise query for candidates and disambiguate.
pub async fn resolve_symbol(
    store: &Arc<dyn GraphStore>,
    name: &str,
) -> Result<SymbolResolution, McpError> {
    if name.contains(':') {
        return Ok(SymbolResolution::Id(NodeId::new(name.to_string())));
    }
    let candidates = store.candidates_by_name(name, 10).await.map_err(to_mcp)?;
    Ok(match candidates.len() {
        0 => SymbolResolution::NotFound,
        1 => SymbolResolution::Id(candidates.into_iter().next().unwrap().id),
        _ => SymbolResolution::Ambiguous(candidates),
    })
}

/// Find repo path: explicit `repo` arg → registry by name/path; or fallback to
/// first registry entry whose `graph_key` matches the server's active key.
pub fn find_repo_path(
    repo: Option<&str>,
    graph_key: &str,
) -> std::result::Result<String, String> {
    let reg = cih_core::Registry::load();
    if reg.entries.is_empty() {
        return Err("no repos in registry — run `cih-engine analyze <repo>` first".to_string());
    }
    if let Some(name_or_path) = repo {
        reg.find(name_or_path)
            .map(|e| e.path.clone())
            .ok_or_else(|| format!("repo '{name_or_path}' not in registry"))
    } else {
        reg.entries
            .iter()
            .find(|e| e.graph_key == graph_key)
            .map(|e| e.path.clone())
            .ok_or_else(|| {
                format!("no repo registered for graph_key '{graph_key}'; pass `repo` explicitly")
            })
    }
}

/// Run `git diff --name-only` and return repo-relative file paths.
pub fn git_changed_files(
    repo_path: &str,
    scope: &str,
    base_ref: Option<&str>,
) -> std::result::Result<Vec<String>, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("diff").arg("--name-only");
    match scope {
        "staged" => {
            cmd.arg("--cached").arg("HEAD");
        }
        "base_ref" => {
            let r = base_ref
                .ok_or_else(|| "`base_ref` scope requires the `base_ref` argument".to_string())?;
            cmd.arg(r);
        }
        _ => {
            cmd.arg("HEAD");
        }
    }
    cmd.current_dir(repo_path);
    let output = cmd.output().map_err(|e| format!("git diff failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git diff error: {stderr}"));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}
