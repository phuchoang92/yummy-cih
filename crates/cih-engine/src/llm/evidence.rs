use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use cih_core::{Node, NodeKind};
use cih_wiki::features::infer_community_feature;
use cih_wiki::graph::{route_http_method, route_path, WikiGraph};

const MAX_EVIDENCE_CHARS: usize = 3_000;

#[derive(Clone, Debug, PartialEq, Eq)]
enum EvidenceKind {
    Route,
    Process,
    CodeShape,
    Dependency,
    Table,
    Event,
    External,
    Snippet,
    Brd,
}

#[derive(Clone, Debug)]
pub struct EvidenceItem {
    pub id: String,
    kind: EvidenceKind,
    pub text: String,
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
            for (idx, chunk) in split_chunks(&text).into_iter().enumerate() {
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

fn safe_repo_path(repo: &Path, rel: &str) -> Option<PathBuf> {
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
        .trim_start_matches("Method:")
        .trim_start_matches("Constructor:")
        .trim_start_matches("Function:");
    fqcn.rsplit('.').next().map(|s| s.to_string())
}

fn split_chunks(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        if current.len() + para.len() + 2 <= 400 {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(para);
        } else {
            if !current.is_empty() {
                chunks.push(current);
                current = String::new();
            }
            if para.len() <= 400 {
                current.push_str(para);
            } else {
                let mut start = 0usize;
                while start < para.len() {
                    let mut end = (start + 400).min(para.len());
                    while end > start && !para.is_char_boundary(end) {
                        end -= 1;
                    }
                    chunks.push(para[start..end].trim().to_string());
                    start = end;
                }
            }
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{Edge, EdgeKind, NodeId, Range};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn node(id: &str, kind: NodeKind, name: &str, file: &str, line: u32) -> Node {
        Node {
            id: NodeId::new(id.to_string()),
            kind,
            name: name.to_string(),
            qualified_name: None,
            file: file.to_string(),
            range: Range {
                start_line: line,
                end_line: line,
                ..Range::default()
            },
            props: None,
        }
    }

    fn temp_repo() -> PathBuf {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("cih-evidence-test-{}-{id}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        root
    }

    #[test]
    fn split_md_and_txt_chunks_at_paragraphs() {
        let chunks = split_chunks("one\n\n two\n\nthree");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].contains("one"));
        assert!(chunks[0].contains("three"));
    }

    #[test]
    fn safe_repo_path_rejects_escaping_paths() {
        let root = temp_repo();
        std::fs::write(root.join("src/Foo.java"), "class Foo {}").unwrap();
        assert!(safe_repo_path(&root, "src/Foo.java").is_some());
        assert!(safe_repo_path(&root, "../secret.java").is_none());
        assert!(safe_repo_path(&root, "/tmp/secret.java").is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn evidence_pack_includes_routes_and_tables() {
        let method = node(
            "Method:com.example.OrderService#find/0",
            NodeKind::Method,
            "find",
            "src/OrderService.java",
            1,
        );
        let community = node("Community:0", NodeKind::Community, "order-service", "", 0);
        let route = Node {
            props: Some(serde_json::json!({"httpMethod": "GET", "path": "/orders"})),
            ..node("Route:GET:/orders", NodeKind::Route, "GET /orders", "", 0)
        };
        let query = node("DbQuery:q", NodeKind::DbQuery, "q", "", 0);
        let table = node("DbTable:ORDERS", NodeKind::DbTable, "ORDERS", "", 0);
        let edges = [
            Edge {
                src: method.id.clone(),
                dst: route.id.clone(),
                kind: EdgeKind::HandlesRoute,
                confidence: 1.0,
                reason: String::new(),
            },
            Edge {
                src: method.id.clone(),
                dst: query.id.clone(),
                kind: EdgeKind::ExecutesQuery,
                confidence: 1.0,
                reason: String::new(),
            },
            Edge {
                src: query.id.clone(),
                dst: table.id.clone(),
                kind: EdgeKind::ReadsTable,
                confidence: 1.0,
                reason: String::new(),
            },
        ];
        let comm_edges = [Edge {
            src: method.id.clone(),
            dst: community.id.clone(),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
        }];
        let graph = WikiGraph::build(
            &[method, route, query, table],
            &edges,
            &[community.clone()],
            &comm_edges,
        );
        let pack = build_evidence_pack(None, &graph, &community, &EvidenceCorpus::default());
        let rendered = pack.render();
        assert!(rendered.contains("[R1] GET /orders"));
        assert!(rendered.contains("[T1] ORDERS (read)"));
    }

    #[test]
    fn evidence_pack_includes_only_business_processes() {
        let community = node("Community:0", NodeKind::Community, "order-service", "", 0);
        let business = Node {
            props: Some(serde_json::json!({
                "label": "Create order",
                "communities": ["Community:0"],
                "business_flow": true,
                "entrypoint_kind": "http_route",
                "step_count": 3,
                "route_method": "POST",
                "route_path": "/orders"
            })),
            ..node(
                "Process:create-order",
                NodeKind::Process,
                "Create order",
                "",
                0,
            )
        };
        let internal = Node {
            props: Some(serde_json::json!({
                "label": "Internal fanout",
                "communities": ["Community:0"],
                "business_flow": false,
                "entrypoint_kind": "fanout",
                "step_count": 4
            })),
            ..node(
                "Process:internal-fanout",
                NodeKind::Process,
                "Internal fanout",
                "",
                0,
            )
        };
        let graph = WikiGraph::build(&[], &[], &[community.clone(), business, internal], &[]);
        let pack = build_evidence_pack(None, &graph, &community, &EvidenceCorpus::default());
        let rendered = pack.render();
        assert!(rendered.contains("[P1] Create order"));
        assert!(rendered.contains("route POST /orders"));
        assert!(!rendered.contains("Internal fanout"));
    }

    #[test]
    fn brd_matching_requires_two_distinct_terms_and_caps_to_two_chunks() {
        let method = node(
            "Method:com.example.OrderService#cancel/0",
            NodeKind::Method,
            "cancel",
            "modules/order/OrderService.java",
            1,
        );
        let community = node("Community:0", NodeKind::Community, "order-service", "", 0);
        let comm_edges = [Edge {
            src: method.id.clone(),
            dst: community.id.clone(),
            kind: EdgeKind::MemberOf,
            confidence: 1.0,
            reason: String::new(),
        }];
        let graph = WikiGraph::build(&[method], &[], &[community.clone()], &comm_edges);
        let corpus = EvidenceCorpus {
            file_count: 1,
            chunks: vec![
                EvidenceChunk {
                    source: "brd.md#1".into(),
                    text: "order service workflow".into(),
                },
                EvidenceChunk {
                    source: "brd.md#2".into(),
                    text: "order service approval".into(),
                },
                EvidenceChunk {
                    source: "brd.md#3".into(),
                    text: "only order".into(),
                },
            ],
        };
        let pack = build_evidence_pack(None, &graph, &community, &corpus);
        let brd_count = pack
            .items
            .iter()
            .filter(|i| i.kind == EvidenceKind::Brd)
            .count();
        assert_eq!(brd_count, 2);
        assert!(pack.render().contains("[B1]"));
        assert!(!pack.render().contains("only order"));
    }

    #[test]
    fn source_snippet_selection_is_deterministic_and_capped() {
        let root = temp_repo();
        std::fs::write(
            root.join("src/A.java"),
            "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n",
        )
        .unwrap();
        std::fs::write(root.join("src/B.java"), "a\nb\nc\nd\ne\n").unwrap();
        let m1 = node("Method:a.A#m1/0", NodeKind::Method, "m1", "src/A.java", 2);
        let m2 = node("Method:a.A#m2/0", NodeKind::Method, "m2", "src/A.java", 4);
        let m3 = node("Method:a.B#m1/0", NodeKind::Method, "m1", "src/B.java", 1);
        let community = node("Community:0", NodeKind::Community, "shared", "", 0);
        let comm_edges = [
            Edge {
                src: m1.id.clone(),
                dst: community.id.clone(),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: String::new(),
            },
            Edge {
                src: m2.id.clone(),
                dst: community.id.clone(),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: String::new(),
            },
            Edge {
                src: m3.id.clone(),
                dst: community.id.clone(),
                kind: EdgeKind::MemberOf,
                confidence: 1.0,
                reason: String::new(),
            },
        ];
        let graph = WikiGraph::build(&[m1, m2, m3], &[], &[community.clone()], &comm_edges);
        let pack = build_evidence_pack(Some(&root), &graph, &community, &EvidenceCorpus::default());
        let rendered = pack.render();
        assert!(rendered.contains("[S1] src/A.java:2-11"));
        assert!(rendered.len() <= MAX_EVIDENCE_CHARS);
        let _ = std::fs::remove_dir_all(root);
    }
}
