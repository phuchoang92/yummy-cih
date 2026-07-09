use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use cih_core::{Edge, Node, NodeKind};
use cih_wiki::graph::route_path;
use cih_wiki::{assign_class_slugs, WikiGraph};

use super::config::WikiArtifacts;

pub(super) fn filter_communities_by_route(
    mut communities: Vec<cih_core::Node>,
    graph: &WikiGraph,
    patterns: &[String],
) -> Vec<cih_core::Node> {
    if patterns.is_empty() {
        return communities;
    }
    let patterns_lower: Vec<String> = patterns.iter().map(|p| p.to_lowercase()).collect();
    let before = communities.len();
    communities.retain(|n| {
        let comm_id = n.id.as_str();
        graph
            .community_routes
            .get(comm_id)
            .map(|routes| {
                routes.iter().any(|(_, route)| {
                    let path = route_path(route).to_lowercase();
                    patterns_lower
                        .iter()
                        .any(|pat| path.starts_with(pat.as_str()) || path.contains(pat.as_str()))
                })
            })
            .unwrap_or(false)
    });
    if communities.len() != before {
        tracing::info!(
            before = before,
            after = communities.len(),
            patterns = ?patterns,
            "route filter applied"
        );
        eprintln!(
            "info: --filter-route matched {} of {} communities",
            communities.len(),
            before
        );
    }
    communities
}

fn first_meaningful_route_seg(path: &str) -> Option<String> {
    const GENERIC: &[&str] = &[
        "api", "apis", "rest", "internal", "external", "service", "services", "common", "shared",
        "core", "app", "apps", "admin", "pos", "public", "private",
    ];
    for part in path.split('/') {
        let part = part.trim();
        if part.is_empty() || part.starts_with('{') || part.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let mut chars = part.chars();
        if matches!(chars.next(), Some('v') | Some('V')) && chars.all(|c| c.is_ascii_digit()) {
            continue;
        }
        let lower = part.to_lowercase();
        if GENERIC.contains(&lower.as_str()) {
            continue;
        }
        return Some(lower);
    }
    None
}

pub fn community_matches_route_prefix(community: &Node, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return true;
    }
    let Some(props) = &community.props else {
        return true;
    };
    let Some(arr) = props.get("route_prefixes").and_then(|v| v.as_array()) else {
        return true;
    };
    if arr.is_empty() {
        return false;
    }
    let prefixes: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    patterns.iter().any(|pat| {
        let Some(pat_seg) = first_meaningful_route_seg(pat) else {
            return true;
        };
        prefixes.iter().any(|p| {
            let p_lower = p.to_lowercase();
            p_lower == pat_seg
                || p_lower.contains(pat_seg.as_str())
                || pat_seg.contains(p_lower.as_str())
        })
    })
}

pub(super) fn build_file_dev_map(
    nodes: &[Node],
    feature_of: &dyn Fn(&str, &str) -> String,
) -> HashMap<String, String> {
    use std::collections::BTreeSet;

    let mut by_feature: std::collections::BTreeMap<String, BTreeSet<String>> =
        std::collections::BTreeMap::new();
    let mut id_to_name: HashMap<String, String> = HashMap::new();
    let mut id_to_file: HashMap<String, String> = HashMap::new();

    for node in nodes {
        if !matches!(
            node.kind,
            NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Record
        ) || node.file.is_empty()
        {
            continue;
        }
        let id = node.id.as_str().to_string();
        let feature = feature_of(id.as_str(), node.file.as_str());
        by_feature.entry(feature).or_default().insert(id.clone());
        id_to_name
            .entry(id.clone())
            .or_insert_with(|| node.name.clone());
        id_to_file.entry(id).or_insert_with(|| node.file.clone());
    }

    let mut file_to_url: HashMap<String, String> = HashMap::new();
    for (feature, class_ids) in by_feature {
        let slugs = assign_class_slugs(&class_ids, |id| {
            id_to_name.get(id).cloned().unwrap_or_else(|| {
                id.trim_start_matches("Class:")
                    .rsplit('.')
                    .next()
                    .unwrap_or("Unknown")
                    .to_string()
            })
        });
        for (class_id, slug) in slugs {
            if let Some(file) = id_to_file.get(&class_id) {
                let url = format!("/docs/{}/dev/{}", feature, slug);
                file_to_url.entry(file.clone()).or_insert(url);
            }
        }
    }
    file_to_url
}

pub(super) fn load_wiki_artifacts(
    repo: &Path,
    out: Option<PathBuf>,
    grouping: super::config::WikiGrouping,
    filter_community: &[String],
    max_communities: Option<usize>,
    filter_route: &[String],
) -> Result<Option<WikiArtifacts>> {
    let graph_artifacts;
    let nodes;
    let edges;
    let wiki_graph;
    let community_nodes: Vec<Node>;
    let community_edges: Vec<Edge>;
    let community_version: String;
    #[allow(clippy::type_complexity)] // LLM plumbing signature; alias with wiki rework
    let feature_of: Box<dyn Fn(&str, &str) -> String + Send>;

    if grouping == super::config::WikiGrouping::Package {
        graph_artifacts = crate::versioning::latest_graph_artifacts(repo)?;
        nodes = graph_artifacts.read_nodes().with_context(|| {
            format!(
                "failed to read nodes from {}",
                graph_artifacts.nodes_path.display()
            )
        })?;
        edges = graph_artifacts.read_edges().with_context(|| {
            format!(
                "failed to read edges from {}",
                graph_artifacts.edges_path.display()
            )
        })?;
        tracing::info!(
            graph_version = %graph_artifacts.version,
            nodes = nodes.len(),
            edges = edges.len(),
            "graph artifacts loaded (package mode)"
        );
        let pkg_cfg = cih_grouping::PackageConfig::load_or_default(repo);
        let pkg_strategy: Arc<dyn cih_grouping::FeatureStrategy> =
            Arc::new(cih_grouping::PackageStrategy::new(pkg_cfg));

        let repo_default_feature: Arc<String> = Arc::new({
            let raw = repo
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("shared")
                .to_lowercase();
            let mut s = raw.as_str();
            for suf in &[
                "-api", "-service", "-impl", "-core", "-module", "-web", "-rest",
            ] {
                s = s.strip_suffix(suf).unwrap_or(s);
            }
            for pfx in &[
                "banking-",
                "payment-",
                "finance-",
                "base-",
                "common-",
                "core-",
                "shared-",
                "platform-",
                "infra-",
                "app-",
                "service-",
            ] {
                s = s.strip_prefix(pfx).unwrap_or(s);
            }
            if s.is_empty() || s == "shared" {
                raw
            } else {
                s.to_string()
            }
        });

        let feature_lookup: Arc<std::collections::HashMap<String, String>> = Arc::new(
            cih_grouping::find_feature_artifact_dir(repo, graph_artifacts.version.as_str())
                .and_then(|dir| cih_grouping::read_feature_artifact(&dir).ok())
                .map(|entries| entries.into_iter().map(|e| (e.node_id, e.name)).collect())
                .unwrap_or_default(),
        );
        if !feature_lookup.is_empty() {
            tracing::info!(
                entries = feature_lookup.len(),
                "loaded pre-computed feature artifact"
            );
        }

        {
            let s = pkg_strategy.clone();
            let lk = feature_lookup.clone();
            let df = repo_default_feature.clone();
            wiki_graph = WikiGraph::build_package_grouped(&nodes, &edges, &|node_id, f| {
                let feat = lk.get(node_id).cloned().unwrap_or_else(|| s.feature_of(f));
                if feat == "shared" {
                    df.as_ref().clone()
                } else {
                    feat
                }
            });
        }
        let all_pkg_nodes: Vec<Node> = wiki_graph.community_nodes.clone();
        community_nodes = filter_communities_by_route(all_pkg_nodes, &wiki_graph, filter_route);
        if !filter_route.is_empty() && community_nodes.is_empty() {
            eprintln!("info: --filter-route matched 0 packages; nothing to generate.");
            return Ok(None);
        }
        community_edges = Vec::new();
        community_version = graph_artifacts.version.to_string();
        feature_of = Box::new(move |node_id: &str, f: &str| {
            let feat = feature_lookup
                .get(node_id)
                .cloned()
                .unwrap_or_else(|| pkg_strategy.feature_of(f));
            if feat == "shared" {
                repo_default_feature.as_ref().clone()
            } else {
                feat
            }
        });
    } else {
        let community_artifact =
            cih_core::GraphArtifacts::latest_in_dir(&repo.join(".cih").join("artifacts-community"))
                .ok();
        let (pre_community_nodes, community_version_raw) = match community_artifact.as_ref() {
            Some(a) => {
                let ns = a.read_nodes().with_context(|| {
                    format!(
                        "failed to read community nodes from {}",
                        a.nodes_path.display()
                    )
                })?;
                let ver = a.version.to_string();
                tracing::info!(
                    community_version = %ver,
                    communities = ns.len(),
                    "community artifacts loaded"
                );
                (ns, ver)
            }
            None => {
                tracing::info!(
                    "no community artifacts found — generating wiki without feature grouping; \
                     run `discover` first for richer docs"
                );
                eprintln!(
                    "info: no community artifacts found — generating wiki without feature grouping. \
                     Run `discover` first for richer docs."
                );
                (Vec::new(), String::new())
            }
        };

        let community_nodes_pre: Vec<Node> = {
            let before = pre_community_nodes.len();
            let mut filtered = pre_community_nodes;
            if !filter_community.is_empty() {
                let filters_lower: Vec<String> =
                    filter_community.iter().map(|f| f.to_lowercase()).collect();
                filtered.retain(|n| {
                    let name_lower = n.name.to_lowercase();
                    filters_lower
                        .iter()
                        .any(|f| name_lower.contains(f.as_str()))
                });
            }
            if let Some(max) = max_communities {
                filtered.truncate(max);
            }
            if filtered.len() != before {
                tracing::info!(
                    before = before,
                    after = filtered.len(),
                    filter_community = ?filter_community,
                    max_communities = ?max_communities,
                    "community filter applied"
                );
            }
            filtered
        };

        let community_nodes_pre: Vec<Node> = if !filter_route.is_empty() {
            community_nodes_pre
                .into_iter()
                .filter(|n| community_matches_route_prefix(n, filter_route))
                .collect()
        } else {
            community_nodes_pre
        };

        if !filter_route.is_empty()
            && community_artifact.is_some()
            && community_nodes_pre.is_empty()
        {
            eprintln!(
                "info: --filter-route matched 0 communities (pre-filter); nothing to generate."
            );
            return Ok(None);
        }

        graph_artifacts = crate::versioning::latest_graph_artifacts(repo)?;
        nodes = graph_artifacts.read_nodes().with_context(|| {
            format!(
                "failed to read nodes from {}",
                graph_artifacts.nodes_path.display()
            )
        })?;
        edges = graph_artifacts.read_edges().with_context(|| {
            format!(
                "failed to read edges from {}",
                graph_artifacts.edges_path.display()
            )
        })?;
        tracing::info!(
            graph_version = %graph_artifacts.version,
            nodes = nodes.len(),
            edges = edges.len(),
            "graph artifacts loaded"
        );

        let (community_nodes_loaded, community_edges_loaded, cv) = match community_artifact {
            Some(a) => {
                let comm_edges = a.read_edges().with_context(|| {
                    format!(
                        "failed to read community edges from {}",
                        a.edges_path.display()
                    )
                })?;
                (community_nodes_pre, comm_edges, community_version_raw)
            }
            None => (Vec::new(), Vec::new(), String::new()),
        };
        community_version = cv;

        wiki_graph = WikiGraph::build(
            &nodes,
            &edges,
            &community_nodes_loaded,
            &community_edges_loaded,
        );
        community_edges = community_edges_loaded;
        community_nodes =
            filter_communities_by_route(community_nodes_loaded, &wiki_graph, filter_route);
        feature_of = Box::new(|_, _| "shared".to_string());
    }

    let bodies = {
        let member_ids: std::collections::HashSet<&str> = community_nodes
            .iter()
            .flat_map(|c| {
                wiki_graph
                    .members_by_community
                    .get(c.id.as_str())
                    .into_iter()
                    .flatten()
                    .map(|n| n.id.as_str())
            })
            .collect();
        let body_nodes: Vec<Node> = nodes
            .iter()
            .filter(|n| member_ids.contains(n.id.as_str()))
            .cloned()
            .collect();
        cih_wiki::source_bodies(&body_nodes, repo)
    };

    let repo_map_path = repo.join(".cih").join("repo-map.json");
    let repo_map: Option<cih_core::RepoMap> = if repo_map_path.is_file() {
        let content = std::fs::read_to_string(&repo_map_path)
            .with_context(|| format!("failed to read {}", repo_map_path.display()))?;
        Some(
            serde_json::from_str(&content)
                .with_context(|| format!("failed to parse {}", repo_map_path.display()))?,
        )
    } else {
        None
    };

    let unresolved_path = graph_artifacts
        .nodes_path
        .parent()
        .map(|p| p.join("unresolved-refs.md"));
    let unresolved_report: Option<String> = unresolved_path.and_then(|p| {
        if p.is_file() {
            std::fs::read_to_string(&p).ok()
        } else {
            None
        }
    });

    let out_dir = out.unwrap_or_else(|| repo.join(".cih").join("wiki"));
    let repo_name = std::fs::canonicalize(repo)
        .unwrap_or_else(|_| repo.to_path_buf())
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    let file_dev_map = build_file_dev_map(&nodes, &*feature_of);

    Ok(Some(WikiArtifacts {
        nodes,
        edges,
        wiki_graph,
        community_nodes,
        community_edges,
        community_version,
        graph_version: graph_artifacts.version.to_string(),
        repo_map,
        unresolved_report,
        out_dir,
        repo_name,
        bodies,
        file_dev_map,
        feature_of,
    }))
}
