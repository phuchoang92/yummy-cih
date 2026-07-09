use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use cih_core::{Node, NodeKind};
use cih_wiki::features::infer_community_feature;
use cih_wiki::graph::{route_http_method, route_path, WikiGraph};

pub const MAX_EVIDENCE_CHARS: usize = 3_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvidenceKind {
    Route,
    Process,
    CodeShape,
    Dependency,
    Table,
    Event,
    External,
    IntegrationRoute,
    MessageDestination,
    Snippet,
    Brd,
}

#[derive(Clone, Debug)]
pub struct EvidenceItem {
    pub id: String,
    pub kind: EvidenceKind,
    pub text: String,
}

impl EvidenceItem {
    /// True when this item is a source-code snippet (id starts with "S").
    pub fn is_snippet(&self) -> bool {
        matches!(self.kind, EvidenceKind::Snippet)
    }

    /// For snippet items, returns the relative file path (strips the line-range suffix).
    pub fn snippet_file(&self) -> Option<&str> {
        if !self.is_snippet() {
            return None;
        }
        // text format: "src/main/.../Foo.java:41-50\n    ..."
        self.text
            .lines()
            .next()
            .and_then(|first| first.split(':').next())
    }
}

#[derive(Clone, Debug, Default)]
pub struct EvidencePack {
    pub community_id: String,
    pub community_name: String,
    pub items: Vec<EvidenceItem>,
}

impl EvidencePack {
    pub fn render(&self) -> String {
        self.items
            .iter()
            .filter(|item| !item.text.trim().is_empty())
            .map(|item| format!("[{}] {}", item.id, item.text.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug)]
pub struct EvidenceChunk {
    pub source: String,
    pub text: String,
}

#[derive(Clone, Debug, Default)]
pub struct EvidenceCorpus {
    pub chunks: Vec<EvidenceChunk>,
    pub file_count: usize,
}

impl EvidenceCorpus {
    pub fn load(paths: &[PathBuf]) -> Result<Self> {
        let mut chunks = Vec::new();
        for path in paths {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext != "md" && ext != "txt" {
                bail!(
                    "unsupported evidence file {} (expected .md or .txt)",
                    path.display()
                );
            }
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read evidence file {}", path.display()))?;
            for (idx, chunk) in super::split_text_chunks(&text, 400).into_iter().enumerate() {
                chunks.push(EvidenceChunk {
                    source: format!("{}#{}", path.display(), idx + 1),
                    text: chunk,
                });
            }
        }
        Ok(Self {
            chunks,
            file_count: paths.len(),
        })
    }
}

pub fn build_evidence_pack(
    repo: Option<&Path>,
    graph: &WikiGraph,
    community: &Node,
    corpus: &EvidenceCorpus,
) -> EvidencePack {
    let comm_id = community.id.as_str();
    let mut items = Vec::new();

    push_routes(&mut items, graph, comm_id);
    push_processes(&mut items, graph, comm_id);
    push_stereotypes(&mut items, graph, comm_id);
    push_dependencies(&mut items, graph, comm_id);
    push_tables(&mut items, graph, comm_id);
    push_events(&mut items, graph, comm_id);
    push_external_calls(&mut items, graph, comm_id);
    push_integration_routes(&mut items, graph, comm_id);
    push_message_destinations(&mut items, graph, comm_id);
    if let Some(repo) = repo {
        push_source_snippets(&mut items, repo, graph, comm_id);
    }
    push_brd_chunks(&mut items, graph, community, corpus);

    enforce_size_cap(&mut items);

    EvidencePack {
        community_id: comm_id.to_string(),
        community_name: community.name.clone(),
        items,
    }
}

fn push_routes(items: &mut Vec<EvidenceItem>, graph: &WikiGraph, comm_id: &str) {
    if let Some(routes) = graph.community_routes.get(comm_id) {
        for (idx, (_, route)) in routes.iter().enumerate() {
            items.push(EvidenceItem {
                id: format!("R{}", idx + 1),
                kind: EvidenceKind::Route,
                text: format!("{} {}", route_http_method(route), route_path(route)),
            });
        }
    }
}

fn push_processes(items: &mut Vec<EvidenceItem>, graph: &WikiGraph, comm_id: &str) {
    let mut idx = 0usize;
    for proc in &graph.process_nodes {
        let props = proc.props.as_ref();
        let business_flow = props
            .and_then(|p| p.get("business_flow"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !business_flow {
            continue;
        }
        let touches = proc
            .props
            .as_ref()
            .and_then(|p| p.get("communities"))
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(|c| c.as_str() == Some(comm_id)))
            .unwrap_or(false);
        if !touches {
            continue;
        }
        let label = proc
            .props
            .as_ref()
            .and_then(|p| p.get("label"))
            .and_then(|v| v.as_str())
            .unwrap_or(proc.name.as_str());
        let ek = proc
            .props
            .as_ref()
            .and_then(|p| p.get("entrypoint_kind"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let steps = proc
            .props
            .as_ref()
            .and_then(|p| p.get("step_count"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let route = match (
            props
                .and_then(|p| p.get("route_method"))
                .and_then(|v| v.as_str()),
            props
                .and_then(|p| p.get("route_path"))
                .and_then(|v| v.as_str()),
        ) {
            (Some(method), Some(path)) if !method.is_empty() && !path.is_empty() => {
                format!(", route {method} {path}")
            }
            _ => String::new(),
        };
        let topics = props
            .and_then(|p| p.get("event_topics"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|topics| !topics.is_empty())
            .map(|topics| format!(", topics {}", topics.join(", ")))
            .unwrap_or_default();
        idx += 1;
        items.push(EvidenceItem {
            id: format!("P{idx}"),
            kind: EvidenceKind::Process,
            text: format!("{label} ({ek}, {steps} steps{route}{topics})"),
        });
    }
}

fn push_stereotypes(items: &mut Vec<EvidenceItem>, graph: &WikiGraph, comm_id: &str) {
    if let Some(stereotypes) = graph.community_stereotypes.get(comm_id) {
        if !stereotypes.is_empty() {
            let text = stereotypes
                .iter()
                .map(|(name, count)| format!("{} {}", count, name))
                .collect::<Vec<_>>()
                .join(", ");
            items.push(EvidenceItem {
                id: "C1".into(),
                kind: EvidenceKind::CodeShape,
                text: format!("Class stereotypes: {}", text),
            });
        }
    }
}

fn push_dependencies(items: &mut Vec<EvidenceItem>, graph: &WikiGraph, comm_id: &str) {
    let callers = graph
        .callers_of(comm_id)
        .into_iter()
        .map(|(id, count)| format!("{} ({})", graph.community_name(&id), count))
        .collect::<Vec<_>>();
    let callees = graph
        .callees_of(comm_id)
        .into_iter()
        .map(|(id, count)| format!("{} ({})", graph.community_name(&id), count))
        .collect::<Vec<_>>();
    if !callers.is_empty() || !callees.is_empty() {
        let caller_text = if callers.is_empty() {
            "none".to_string()
        } else {
            callers.join(", ")
        };
        let callee_text = if callees.is_empty() {
            "none".to_string()
        } else {
            callees.join(", ")
        };
        items.push(EvidenceItem {
            id: "D1".into(),
            kind: EvidenceKind::Dependency,
            text: format!("Called by: {}; calls into: {}", caller_text, callee_text),
        });
    }
}

fn push_tables(items: &mut Vec<EvidenceItem>, graph: &WikiGraph, comm_id: &str) {
    if let Some(tables) = graph.community_db_tables.get(comm_id) {
        for (idx, table) in tables.iter().enumerate() {
            let access = match (table.reads, table.writes) {
                (true, true) => "read+write",
                (true, false) => "read",
                (false, true) => "write",
                _ => "unknown",
            };
            items.push(EvidenceItem {
                id: format!("T{}", idx + 1),
                kind: EvidenceKind::Table,
                text: format!("{} ({})", table.table_name, access),
            });
        }
    }
}

fn push_events(items: &mut Vec<EvidenceItem>, graph: &WikiGraph, comm_id: &str) {
    let Some(members) = graph.members_by_community.get(comm_id) else {
        return;
    };
    let mut published = BTreeSet::new();
    let mut listened = BTreeSet::new();
    for member in members {
        if let Some(topic_ids) = graph.publishes.get(member.id.as_str()) {
            for tid in topic_ids {
                published.insert(node_name(graph, tid));
            }
        }
        if let Some(topic_ids) = graph.listens.get(member.id.as_str()) {
            for tid in topic_ids {
                listened.insert(node_name(graph, tid));
            }
        }
    }
    let mut idx = 1usize;
    if !published.is_empty() {
        items.push(EvidenceItem {
            id: format!("E{}", idx),
            kind: EvidenceKind::Event,
            text: format!(
                "Publishes topics/events: {}",
                published.into_iter().collect::<Vec<_>>().join(", ")
            ),
        });
        idx += 1;
    }
    if !listened.is_empty() {
        items.push(EvidenceItem {
            id: format!("E{}", idx),
            kind: EvidenceKind::Event,
            text: format!(
                "Listens to topics/events: {}",
                listened.into_iter().collect::<Vec<_>>().join(", ")
            ),
        });
    }
}

fn push_external_calls(items: &mut Vec<EvidenceItem>, graph: &WikiGraph, comm_id: &str) {
    let Some(members) = graph.members_by_community.get(comm_id) else {
        return;
    };
    let mut endpoints = BTreeSet::new();
    for member in members {
        if let Some(endpoint_ids) = graph.external_calls.get(member.id.as_str()) {
            for eid in endpoint_ids {
                endpoints.insert(node_name(graph, eid));
            }
        }
    }
    if !endpoints.is_empty() {
        items.push(EvidenceItem {
            id: "X1".into(),
            kind: EvidenceKind::External,
            text: format!(
                "External calls: {}",
                endpoints.into_iter().collect::<Vec<_>>().join(", ")
            ),
        });
    }
}

/// Files touched by this community's members. Integration XML nodes are not
/// assigned to communities directly, so we attribute them by shared file.
fn community_files(graph: &WikiGraph, comm_id: &str) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    if let Some(members) = graph.members_by_community.get(comm_id) {
        for member in members {
            if !member.file.is_empty() {
                files.insert(member.file.clone());
            }
        }
    }
    files
}

fn push_integration_routes(items: &mut Vec<EvidenceItem>, graph: &WikiGraph, comm_id: &str) {
    let files = community_files(graph, comm_id);
    if files.is_empty() {
        return;
    }
    let mut idx = 0usize;
    for node in graph.nodes_by_id.values() {
        if node.kind != NodeKind::IntegrationRoute || !files.contains(&node.file) {
            continue;
        }
        let source = node
            .props
            .as_ref()
            .and_then(|p| p.get("source"))
            .and_then(|v| v.as_str())
            .unwrap_or("xml");
        idx += 1;
        items.push(EvidenceItem {
            id: format!("I{idx}"),
            kind: EvidenceKind::IntegrationRoute,
            text: format!("{} ({source} route in {})", node.name, node.file),
        });
    }
}

fn push_message_destinations(items: &mut Vec<EvidenceItem>, graph: &WikiGraph, comm_id: &str) {
    let files = community_files(graph, comm_id);
    if files.is_empty() {
        return;
    }
    let mut idx = 0usize;
    for node in graph.nodes_by_id.values() {
        if node.kind != NodeKind::MessageDestination || !files.contains(&node.file) {
            continue;
        }
        let dest_type = node
            .props
            .as_ref()
            .and_then(|p| p.get("destination_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        idx += 1;
        items.push(EvidenceItem {
            id: format!("M{idx}"),
            kind: EvidenceKind::MessageDestination,
            text: format!(
                "{dest_type}:{} (message destination in {})",
                node.name, node.file
            ),
        });
    }
}

fn push_source_snippets(
    items: &mut Vec<EvidenceItem>,
    repo: &Path,
    graph: &WikiGraph,
    comm_id: &str,
) {
    let Some(members) = graph.members_by_community.get(comm_id) else {
        return;
    };

    let mut by_file: BTreeMap<String, (usize, u32)> = BTreeMap::new();
    for member in members {
        if !matches!(
            member.kind,
            NodeKind::Method | NodeKind::Function | NodeKind::Constructor
        ) || member.file.is_empty()
        {
            continue;
        }
        let entry = by_file.entry(member.file.clone()).or_insert((0, u32::MAX));
        entry.0 += 1;
        if member.range.start_line > 0 {
            entry.1 = entry.1.min(member.range.start_line);
        }
    }

    let mut ranked: Vec<(String, usize, u32)> = by_file
        .into_iter()
        .map(|(file, (count, line))| (file, count, line))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)).then(a.2.cmp(&b.2)));

    for (idx, (file, _, line)) in ranked.into_iter().take(3).enumerate() {
        let Some(path) = safe_repo_path(repo, &file) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let start = line.max(1);
        let end = start + 9;
        let snippet = content
            .lines()
            .enumerate()
            .filter_map(|(i, line_text)| {
                let n = (i + 1) as u32;
                (n >= start && n <= end).then_some(format!("    {}", line_text))
            })
            .collect::<Vec<_>>()
            .join("\n");
        if snippet.trim().is_empty() {
            continue;
        }
        items.push(EvidenceItem {
            id: format!("S{}", idx + 1),
            kind: EvidenceKind::Snippet,
            text: format!("{}:{}-{}\n{}", file, start, end, snippet),
        });
    }
}

fn push_brd_chunks(
    items: &mut Vec<EvidenceItem>,
    graph: &WikiGraph,
    community: &Node,
    corpus: &EvidenceCorpus,
) {
    if corpus.chunks.is_empty() {
        return;
    }
    let terms = community_terms(graph, community);
    let mut matches = corpus
        .chunks
        .iter()
        .filter_map(|chunk| {
            let lower = chunk.text.to_ascii_lowercase();
            let hits = terms
                .iter()
                .filter(|term| lower.contains(term.as_str()))
                .count();
            (hits >= 2).then_some((hits, chunk))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(hits_a, a), (hits_b, b)| {
        hits_b
            .cmp(hits_a)
            .then(a.source.cmp(&b.source))
            .then(a.text.cmp(&b.text))
    });
    for (idx, (_, chunk)) in matches.into_iter().take(2).enumerate() {
        items.push(EvidenceItem {
            id: format!("B{}", idx + 1),
            kind: EvidenceKind::Brd,
            text: format!("{}: {}", chunk.source, chunk.text),
        });
    }
}

fn enforce_size_cap(items: &mut Vec<EvidenceItem>) {
    if render_len(items) <= MAX_EVIDENCE_CHARS {
        return;
    }
    for kind in [EvidenceKind::Brd, EvidenceKind::Snippet] {
        for idx in (0..items.len()).rev() {
            if items[idx].kind != kind {
                continue;
            }
            while render_len(items) > MAX_EVIDENCE_CHARS && items[idx].text.len() > 123 {
                let new_len = items[idx].text.len().saturating_sub(200).max(120);
                items[idx].text.truncate(new_len);
                items[idx].text.push_str("...");
                tracing::debug!(evidence_id = %items[idx].id, "truncated evidence item");
            }
            if render_len(items) <= MAX_EVIDENCE_CHARS {
                return;
            }
        }
    }
    while render_len(items) > MAX_EVIDENCE_CHARS {
        let Some(pos) = items
            .iter()
            .rposition(|i| matches!(i.kind, EvidenceKind::Brd | EvidenceKind::Snippet))
        else {
            break;
        };
        tracing::debug!(evidence_id = %items[pos].id, "dropped evidence item to fit cap");
        items.remove(pos);
    }
}

fn render_len(items: &[EvidenceItem]) -> usize {
    items
        .iter()
        .map(|item| item.id.len() + item.text.len() + 4)
        .sum::<usize>()
}

pub fn safe_repo_path(repo: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return None;
    }
    for component in rel_path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return None;
        }
    }
    let path = repo.join(rel_path);
    let root = repo.canonicalize().ok()?;
    let canonical = path.canonicalize().ok()?;
    canonical.starts_with(&root).then_some(canonical)
}

fn node_name(graph: &WikiGraph, id: &str) -> String {
    graph
        .nodes_by_id
        .get(id)
        .map(|n| n.name.clone())
        .unwrap_or_else(|| id.to_string())
}

fn community_terms(graph: &WikiGraph, community: &Node) -> BTreeSet<String> {
    let mut terms = BTreeSet::new();
    add_terms(&mut terms, &community.name);
    add_terms(
        &mut terms,
        &infer_community_feature(community.id.as_str(), graph),
    );
    if let Some(routes) = graph.community_routes.get(community.id.as_str()) {
        for (_, route) in routes {
            add_terms(&mut terms, &route_path(route));
        }
    }
    if let Some(members) = graph.members_by_community.get(community.id.as_str()) {
        for member in members {
            if matches!(
                member.kind,
                NodeKind::Method | NodeKind::Function | NodeKind::Constructor
            ) {
                if let Some(class_name) = simple_class_from_callable(member.id.as_str()) {
                    add_terms(&mut terms, &class_name);
                }
            } else if matches!(
                member.kind,
                NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Record
            ) {
                add_terms(&mut terms, &member.name);
            }
        }
    }
    terms.retain(|t| t.len() >= 3);
    terms
}

fn add_terms(terms: &mut BTreeSet<String>, text: &str) {
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            terms.insert(current.clone());
            current.clear();
        }
    }
    if !current.is_empty() {
        terms.insert(current);
    }
}

fn simple_class_from_callable(id: &str) -> Option<String> {
    let (prefix, _) = id.split_once('#')?;
    let fqcn = prefix
        .trim_start_matches(crate::node_prefix::METHOD)
        .trim_start_matches(crate::node_prefix::CONSTRUCTOR)
        .trim_start_matches("Function:");
    fqcn.rsplit('.').next().map(|s| s.to_string())
}
