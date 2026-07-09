use std::path::{Path, PathBuf};

use anyhow::Result;
use cih_core::{Edge, JarInfo, Node, RepoMap};
use cih_jar::JarApiExtractor;

use crate::scope::ScopeRequest;

use super::AnalyzeFlags;

/// Extract API-surface nodes+edges from `jars` for the given FQCN set.
/// Demand-driven: only classes matching an unresolved FQCN are parsed.
/// Returns (nodes, edges, failed_jar_count).
pub fn extract_jar_api(jars: &[JarInfo], fqcns: &[String]) -> (Vec<Node>, Vec<Edge>, usize) {
    if fqcns.is_empty() || jars.is_empty() {
        return (Vec::new(), Vec::new(), 0);
    }
    let include: std::collections::HashSet<String> = fqcns.iter().cloned().collect();
    let extractor = JarApiExtractor::with_include(include);
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();
    let mut failed = 0usize;
    for jar in jars {
        match extractor.extract(std::path::Path::new(&jar.path)) {
            Ok(output) => {
                all_nodes.extend(output.nodes);
                all_edges.extend(output.edges);
            }
            Err(err) => {
                tracing::warn!(jar = %jar.path, error = %err, "JAR API extraction failed — skipping");
                failed += 1;
            }
        }
    }
    (all_nodes, all_edges, failed)
}

/// Scan the repo for `*.xml` files and run the integration-XML extractor on each.
/// Best-effort: unreadable files are skipped with a warning, never fatal.
pub fn extract_integration_xml_in_repo(repo_root: &Path) -> (Vec<Node>, Vec<Edge>) {
    use rayon::prelude::*;
    use std::collections::HashSet;

    let xml_files: Vec<PathBuf> = {
        let walker = ignore::WalkBuilder::new(repo_root)
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(true)
            .build();

        walker
            .filter_map(|result| match result {
                Ok(entry) if entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) => {
                    let path = entry.into_path();
                    let is_xml = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("xml"))
                        .unwrap_or(false);
                    if is_xml {
                        Some(path)
                    } else {
                        None
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "integration-xml: walk error — skipping");
                    None
                }
                _ => None,
            })
            .collect()
    };

    let per_file: Vec<_> = xml_files
        .par_iter()
        .filter_map(|path| {
            let rel = path
                .strip_prefix(repo_root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!(file = %rel, error = %err, "integration-xml: read failed — skipping");
                    return None;
                }
            };
            let output = cih_resolve::extract_integration_xml(&rel, &content);
            if output.nodes.is_empty() && output.edges.is_empty() {
                None
            } else {
                Some(output)
            }
        })
        .collect();

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen_node_ids: HashSet<String> = HashSet::new();
    for output in per_file {
        for node in output.nodes {
            if seen_node_ids.insert(node.id.as_str().to_string()) {
                nodes.push(node);
            }
        }
        edges.extend(output.edges);
    }

    (nodes, edges)
}

/// Read `.cih/repo-map.json` and return its JAR catalog. Returns empty on missing/malformed.
pub(super) fn load_jars_from_repo_map(repo: &Path) -> Vec<JarInfo> {
    let path = repo.join(".cih").join("repo-map.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str::<RepoMap>(&raw)
        .map(|rm| rm.jars)
        .unwrap_or_default()
}

pub(super) fn build_scope_request(repo: &Path, flags: &AnalyzeFlags) -> Result<ScopeRequest> {
    let scope_path = if let Some(path) = &flags.scope {
        Some(path.clone())
    } else {
        let default = repo.join("cih.scope.toml");
        default.exists().then_some(default)
    };

    let mut request = if let Some(path) = scope_path {
        ScopeRequest::from_toml(&path)?
    } else {
        ScopeRequest::default()
    };

    if flags.all {
        request.all = true;
        request.modules.clear();
        request.include.clear();
    } else if !flags.modules.is_empty() {
        request.all = false;
        request.modules = flags.modules.clone();
        request.include.clear();
    } else if !flags.include.is_empty() {
        request.all = false;
        request.modules.clear();
        request.include = flags.include.clone();
    }

    if !flags.exclude.is_empty() {
        request.exclude = flags.exclude.clone();
    }
    if flags.include_decompiled {
        request.include_decompiled = true;
    }
    if !flags.languages.is_empty() {
        request.languages = flags.languages.clone();
    }

    Ok(request)
}
