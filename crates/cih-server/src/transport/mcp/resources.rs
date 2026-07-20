//! MCP resource discovery and bounded resource reads.

use cih_core::{EdgeKind, NodeKind};
use rmcp::{
    model::{
        AnnotateAble, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParam,
        RawResource, RawResourceTemplate, ReadResourceRequestParam, ReadResourceResult,
        ResourceContents,
    },
    ErrorData as McpError,
};

use super::error::app_error_to_mcp;
use crate::domain::repository::{RepoCatalogSnapshot, RepoSelector, ResolvedRepo};

fn annotated_resource(uri: &str, name: &str, description: &str) -> rmcp::model::Resource {
    RawResource {
        uri: uri.to_string(),
        name: name.to_string(),
        title: None,
        description: Some(description.to_string()),
        mime_type: Some("application/json".to_string()),
        size: None,
        icons: None,
    }
    .no_annotation()
}

/// Page size for `resources/list`. The full set is small (repos × 4), but honor
/// pagination so discovery clients that page never miss entries.
const RESOURCE_PAGE_SIZE: usize = 100;

/// Build the static list of resources from the registry, paginated by `request`.
pub fn list_resources(
    catalog: &RepoCatalogSnapshot,
    request: Option<PaginatedRequestParam>,
) -> Result<ListResourcesResult, McpError> {
    let mut resources = Vec::new();
    for entry in &catalog.registry().entries {
        let n = &entry.name;
        resources.push(annotated_resource(
            &format!("cih://repo/{n}/context"),
            &format!("{n}/context"),
            &format!("Registry entry and stats for repo '{n}'"),
        ));
        resources.push(annotated_resource(
            &format!("cih://repo/{n}/communities"),
            &format!("{n}/communities"),
            &format!(
                "Community cluster nodes for repo '{n}' (paged: append \
                 ?cursor=...&limit=... — the response carries next_cursor/next_uri)"
            ),
        ));
        resources.push(annotated_resource(
            &format!("cih://repo/{n}/processes"),
            &format!("{n}/processes"),
            &format!(
                "Process trace nodes for repo '{n}' (paged: append \
                 ?cursor=...&limit=... — the response carries next_cursor/next_uri)"
            ),
        ));
        resources.push(annotated_resource(
            &format!("cih://repo/{n}/schema"),
            &format!("{n}/schema"),
            "Graph schema: node kinds and edge kinds",
        ));
    }
    paginate_resources(resources, request.and_then(|r| r.cursor))
}

/// Page a stable-ordered resource list. The cursor is a decimal offset into the
/// list; `next_cursor` is set only when more resources remain. A non-numeric
/// cursor is an invalid-params error; an offset at/after the end yields an empty
/// final page (a client that paged to the end and asked again gets `[]`).
fn paginate_resources(
    all: Vec<rmcp::model::Resource>,
    cursor: Option<String>,
) -> Result<ListResourcesResult, McpError> {
    let total = all.len();
    let offset = match cursor {
        Some(c) => c.parse::<usize>().map_err(|_| {
            McpError::invalid_params(format!("invalid resources/list cursor: {c:?}"), None)
        })?,
        None => 0,
    };
    let resources: Vec<_> = all
        .into_iter()
        .skip(offset)
        .take(RESOURCE_PAGE_SIZE)
        .collect();
    let next = offset.saturating_add(resources.len());
    let next_cursor = (next < total).then(|| next.to_string());
    Ok(ListResourcesResult {
        next_cursor,
        resources,
    })
}

/// Build resource templates (URI patterns) for dynamic lookup.
pub fn list_resource_templates(
    _request: Option<PaginatedRequestParam>,
) -> Result<ListResourceTemplatesResult, McpError> {
    let templates = vec![
        RawResourceTemplate {
            uri_template: "cih://repo/{name}/context".to_string(),
            name: "repo-context".to_string(),
            title: Some("Repo context".to_string()),
            description: Some("Registry entry for an indexed repo".to_string()),
            mime_type: Some("application/json".to_string()),
        }
        .no_annotation(),
        RawResourceTemplate {
            uri_template: "cih://repo/{name}/communities".to_string(),
            name: "repo-communities".to_string(),
            title: Some("Repo communities".to_string()),
            description: Some(
                "Community cluster nodes for an indexed repo — paged: the bare URI is the \
                 first page; follow the response's next_uri (?cursor=...&limit=...) for more"
                    .to_string(),
            ),
            mime_type: Some("application/json".to_string()),
        }
        .no_annotation(),
        RawResourceTemplate {
            uri_template: "cih://repo/{name}/processes".to_string(),
            name: "repo-processes".to_string(),
            title: Some("Repo processes".to_string()),
            description: Some(
                "Process trace nodes for an indexed repo — paged: the bare URI is the \
                 first page; follow the response's next_uri (?cursor=...&limit=...) for more"
                    .to_string(),
            ),
            mime_type: Some("application/json".to_string()),
        }
        .no_annotation(),
        RawResourceTemplate {
            uri_template: "cih://repo/{name}/schema".to_string(),
            name: "repo-schema".to_string(),
            title: Some("Graph schema".to_string()),
            description: Some("Node kinds and edge kinds".to_string()),
            mime_type: Some("application/json".to_string()),
        }
        .no_annotation(),
        RawResourceTemplate {
            uri_template: "cih://repo/{name}/wiki/{slug}".to_string(),
            name: "repo-wiki-page".to_string(),
            title: Some("Wiki page".to_string()),
            description: Some(
                "One generated wiki page (markdown) by slug — find slugs with search_wiki"
                    .to_string(),
            ),
            mime_type: Some("text/markdown".to_string()),
        }
        .no_annotation(),
    ];
    Ok(ListResourceTemplatesResult::with_all_items(templates))
}

/// Serve one resource by URI.
pub fn read_resource(
    catalog: &RepoCatalogSnapshot,
    request: ReadResourceRequestParam,
) -> Result<ReadResourceResult, McpError> {
    let uri = &request.uri;

    // Parse cih://repo/{name}/{section}[?cursor=...&limit=...]
    let rest = uri
        .strip_prefix("cih://repo/")
        .ok_or_else(|| McpError::invalid_params(format!("unknown URI scheme: {uri}"), None))?;

    // Split the pagination query off before any path parsing so sections and
    // wiki slugs never see it.
    let (rest, query) = match rest.split_once('?') {
        Some((path, q)) => (path, Some(q)),
        None => (rest, None),
    };
    let page = parse_page_query(query)?;

    // Wiki page URIs must be handled before the generic name/section split:
    // slugs contain slashes, which rsplit_once would misattribute to the name.
    if let Some((name, slug)) = split_wiki_uri(rest) {
        if query.is_some() {
            return Err(McpError::invalid_params(
                "wiki page resources are not paged — drop the query string".to_string(),
                None,
            ));
        }
        let repo = catalog
            .resolve(RepoSelector::NameOrPath(name.to_string()))
            .map_err(app_error_to_mcp)?;
        return read_wiki_page(&repo, slug, uri);
    }

    let (name, section) = rest
        .rsplit_once('/')
        .ok_or_else(|| McpError::invalid_params(format!("malformed CIH URI: {uri}"), None))?;

    let repo = catalog
        .resolve(RepoSelector::NameOrPath(name.to_string()))
        .map_err(app_error_to_mcp)?;
    let entry = &repo.registry_entry;

    let text = match section {
        "communities" => {
            read_community_nodes(&repo, NodeKind::Community, "communities", uri, &page)?
        }
        "processes" => read_community_nodes(&repo, NodeKind::Process, "processes", uri, &page)?,
        section if query.is_some() => {
            return Err(McpError::invalid_params(
                format!("section '{section}' is not paged — drop the query string"),
                None,
            ));
        }
        "context" => serde_json::to_string_pretty(entry)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?,
        "schema" => schema_json(),
        _ => {
            return Err(McpError::invalid_params(
                format!("unknown section '{section}'"),
                None,
            ));
        }
    };

    Ok(ReadResourceResult {
        contents: vec![ResourceContents::text(text, uri)],
    })
}

/// Pagination options parsed from a resource URI's query string.
struct PageQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, PartialEq, Eq)]
struct ResourceCursor {
    version: String,
    section: String,
    offset: usize,
}

impl ResourceCursor {
    const SCHEMA: &'static str = "v1";

    fn encode(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            Self::SCHEMA,
            self.version,
            self.section,
            self.offset
        )
    }

    fn decode(raw: &str) -> Result<Self, McpError> {
        let malformed = || {
            McpError::invalid_params(
                format!("malformed resource cursor '{raw}' - use the response's next_cursor"),
                None,
            )
        };
        let mut parts = raw.split(':');
        let schema = parts.next().ok_or_else(malformed)?;
        let version = parts
            .next()
            .filter(|v| !v.is_empty())
            .ok_or_else(malformed)?;
        let section = parts
            .next()
            .filter(|v| !v.is_empty())
            .ok_or_else(malformed)?;
        let offset = parts
            .next()
            .ok_or_else(malformed)?
            .parse()
            .map_err(|_| malformed())?;
        if schema != Self::SCHEMA || parts.next().is_some() {
            return Err(malformed());
        }
        Ok(Self {
            version: version.to_string(),
            section: section.to_string(),
            offset,
        })
    }
}

fn parse_page_query(query: Option<&str>) -> Result<PageQuery, McpError> {
    let mut page = PageQuery {
        cursor: None,
        limit: None,
    };
    let Some(query) = query else { return Ok(page) };
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some(("cursor", v)) if !v.is_empty() => page.cursor = Some(v.to_string()),
            Some(("limit", v)) => {
                let n: usize = v.parse().map_err(|_| {
                    McpError::invalid_params(format!("invalid resource limit '{v}'"), None)
                })?;
                if n == 0 {
                    return Err(McpError::invalid_params(
                        "resource limit must be >= 1".to_string(),
                        None,
                    ));
                }
                page.limit = Some(n);
            }
            _ => {
                return Err(McpError::invalid_params(
                    format!("unknown resource query parameter '{pair}' (supported: cursor, limit)"),
                    None,
                ));
            }
        }
    }
    Ok(page)
}

/// Split `"{name}/wiki/{slug}"` on the FIRST `/wiki/` into `(name, slug)`.
/// Registry repo names never contain slashes, so first-occurrence is correct
/// even when the slug itself contains a `wiki` segment.
pub fn split_wiki_uri(rest: &str) -> Option<(&str, &str)> {
    rest.split_once("/wiki/")
        .filter(|(name, slug)| !name.is_empty() && !slug.is_empty())
}

/// Serve one wiki page as markdown. Uncached like the other resource reads:
/// registry → `<repo>/.cih/wiki/manifest.json` → slug lookup → page file.
fn read_wiki_page(
    repo: &ResolvedRepo,
    slug: &str,
    uri: &str,
) -> Result<ReadResourceResult, McpError> {
    let name = &repo.registry_entry.name;
    let wiki_dir = repo.canonical_path.join(".cih").join("wiki");
    let manifest_raw = std::fs::read_to_string(wiki_dir.join("manifest.json")).map_err(|_| {
        McpError::invalid_params(
            format!("no generated wiki for '{name}' — run `cih-engine wiki` first"),
            None,
        )
    })?;
    let manifest: crate::infrastructure::wiki_repository::Manifest =
        serde_json::from_str(&manifest_raw)
        .map_err(|e| McpError::internal_error(format!("invalid wiki manifest: {e}"), None))?;
    let page = manifest
        .pages
        .iter()
        .find(|page| page.slug == slug)
        .ok_or_else(|| {
            McpError::invalid_params(
                format!("no wiki page '{slug}' in repo '{name}' — find slugs with search_wiki"),
                None,
            )
        })?;
    let markdown =
        crate::infrastructure::wiki_repository::read_page_raw(&wiki_dir, &page.path).ok_or_else(|| {
        McpError::internal_error(
            format!("wiki page '{slug}' exists in the manifest but its file is unreadable"),
            None,
        )
        })?;
    Ok(ReadResourceResult {
        contents: vec![ResourceContents::text(markdown, uri)],
    })
}

/// Default and hard-max page sizes for paged resource bodies (design record
/// §11.2).
const RESOURCE_ITEM_DEFAULT: usize = 100;
const RESOURCE_ITEM_MAX: usize = 500;

/// Byte budget per resource page, read once from `CIH_RESOURCE_MAX_BYTES`
/// (unset/invalid/0 = 256 KiB). A page stops before exceeding it and hands
/// back `next_cursor` instead.
fn resource_byte_budget() -> usize {
    static BUDGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("CIH_RESOURCE_MAX_BYTES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or(256 * 1024)
    })
}

/// One bounded page of Community/Process nodes. The artifact is never
/// collected whole (S3): the JSONL is streamed and only the current page is
/// retained, capped by item limit and byte budget. The cursor is stamped with
/// the versioned artifacts dir's basename so a cursor minted before a re-index
/// fails loudly instead of silently paging a different dataset. Order is file
/// order — deterministic per artifact version.
fn read_community_nodes(
    repo: &ResolvedRepo,
    kind: NodeKind,
    section: &str,
    resource_uri: &str,
    page: &PageQuery,
) -> Result<String, McpError> {
    let dir = repo
        .community_artifacts_dir
        .as_deref()
        .ok_or_else(|| McpError::invalid_params("discover not run for this repo yet", None))?;
    let version = std::path::Path::new(dir)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unversioned".to_string());
    let offset = match &page.cursor {
        None => 0,
        Some(c) => {
            let cursor = ResourceCursor::decode(c)?;
            if cursor.version != version {
                return Err(McpError::invalid_params(
                    format!(
                        "stale cursor: artifacts were re-indexed (cursor version '{}', \
                         current '{version}') - restart from the base URI",
                        cursor.version
                    ),
                    None,
                ));
            }
            if cursor.section != section {
                return Err(McpError::invalid_params(
                    format!(
                        "resource cursor is for '{}' but was used with '{section}' - \
                         restart from the base URI",
                        cursor.section
                    ),
                    None,
                ));
            }
            cursor.offset
        }
    };
    let limit = page
        .limit
        .unwrap_or(RESOURCE_ITEM_DEFAULT)
        .min(RESOURCE_ITEM_MAX);
    let path = std::path::Path::new(dir).join("nodes.jsonl");
    let scan = scan_jsonl_candidates(&path, kind.label(), offset, limit.saturating_add(1))?;
    render_resource_page(
        &repo.registry_entry,
        section,
        &version,
        offset,
        limit,
        &scan.items,
        resource_uri,
        resource_byte_budget(),
    )
}

/// A bounded candidate window from a JSONL file. At most `limit + 1` matching
/// records are retained so the response builder can distinguish a full final
/// page from a page with more data.
struct JsonlCandidates {
    items: Vec<serde_json::Value>,
}

fn scan_jsonl_candidates(
    path: &std::path::Path,
    label: &str,
    offset: usize,
    candidate_limit: usize,
) -> Result<JsonlCandidates, McpError> {
    use std::io::BufRead;
    let read_err =
        |e| McpError::internal_error(format!("cannot read {}: {e}", path.display()), None);
    let file = std::fs::File::open(path).map_err(read_err)?;
    let reader = std::io::BufReader::new(file);
    let mut items = Vec::new();
    let mut matched = 0usize;
    for line in reader.lines() {
        let line = line.map_err(read_err)?;
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("kind").and_then(|k| k.as_str()) != Some(label) {
            continue;
        }
        let index = matched;
        matched += 1;
        if index < offset {
            continue;
        }
        if items.len() >= candidate_limit {
            break;
        }
        items.push(v);
    }
    Ok(JsonlCandidates { items })
}

#[derive(serde::Serialize)]
struct ResourcePageBody<'a> {
    items: &'a [serde_json::Value],
    count: usize,
    truncated: bool,
    truncated_by: Option<&'static str>,
    next_cursor: Option<String>,
    next_uri: Option<String>,
    source_version: &'a str,
}

#[allow(clippy::too_many_arguments)]
fn serialize_resource_page(
    entry: &cih_core::RegistryEntry,
    section: &str,
    version: &str,
    offset: usize,
    limit: usize,
    items: &[serde_json::Value],
    available: usize,
) -> Result<String, McpError> {
    let count = items.len();
    let has_more = count < available;
    let truncated_by = if count < available.min(limit) {
        Some("bytes")
    } else if has_more {
        Some("limit")
    } else {
        None
    };
    let next_cursor = has_more.then(|| {
        ResourceCursor {
            version: version.to_string(),
            section: section.to_string(),
            offset: offset + count,
        }
        .encode()
    });
    let next_uri = next_cursor.as_ref().map(|cursor| {
        format!(
            "cih://repo/{}/{section}?cursor={cursor}&limit={limit}",
            entry.name
        )
    });
    serde_json::to_string(&ResourcePageBody {
        items,
        count,
        truncated: has_more,
        truncated_by,
        next_cursor,
        next_uri,
        source_version: version,
    })
    .map_err(|e| McpError::internal_error(e.to_string(), None))
}

#[allow(clippy::too_many_arguments)]
fn render_resource_page(
    entry: &cih_core::RegistryEntry,
    section: &str,
    version: &str,
    offset: usize,
    limit: usize,
    candidates: &[serde_json::Value],
    resource_uri: &str,
    byte_budget: usize,
) -> Result<String, McpError> {
    let available = candidates.len();
    let max_count = available.min(limit);

    // Find the largest prefix whose actual serialized response, including
    // pagination metadata, fits the configured budget.
    let mut low = 0usize;
    let mut high = max_count;
    while low < high {
        let count = (low + high).div_ceil(2);
        let serialized = serialize_resource_page(
            entry,
            section,
            version,
            offset,
            limit,
            &candidates[..count],
            available,
        )?;
        if serialized_resource_result_len(&serialized, resource_uri)? <= byte_budget {
            low = count;
        } else {
            high = count - 1;
        }
    }

    if max_count > 0 && low == 0 {
        return Err(McpError::internal_error(
            format!(
                "first {section} record exceeds CIH_RESOURCE_MAX_BYTES ({byte_budget}); \
                 increase CIH_RESOURCE_MAX_BYTES or regenerate a smaller artifact record"
            ),
            None,
        ));
    }

    let serialized = serialize_resource_page(
        entry,
        section,
        version,
        offset,
        limit,
        &candidates[..low],
        available,
    )?;
    if serialized_resource_result_len(&serialized, resource_uri)? > byte_budget {
        return Err(McpError::internal_error(
            format!("{section} resource metadata exceeds CIH_RESOURCE_MAX_BYTES ({byte_budget})"),
            None,
        ));
    }
    Ok(serialized)
}

fn serialized_resource_result_len(body: &str, resource_uri: &str) -> Result<usize, McpError> {
    serde_json::to_vec(&ReadResourceResult {
        contents: vec![ResourceContents::text(body.to_string(), resource_uri)],
    })
    .map(|bytes| bytes.len())
    .map_err(|e| McpError::internal_error(e.to_string(), None))
}

fn schema_json() -> String {
    #[derive(serde::Serialize)]
    struct Schema {
        node_kinds: Vec<&'static str>,
        edge_kinds: Vec<&'static str>,
    }
    let schema = Schema {
        node_kinds: vec![
            NodeKind::File.label(),
            NodeKind::Folder.label(),
            NodeKind::Class.label(),
            NodeKind::Interface.label(),
            NodeKind::Enum.label(),
            NodeKind::Record.label(),
            NodeKind::Annotation.label(),
            NodeKind::Method.label(),
            NodeKind::Function.label(),
            NodeKind::Constructor.label(),
            NodeKind::Field.label(),
            NodeKind::Route.label(),
            NodeKind::Community.label(),
            NodeKind::Process.label(),
            NodeKind::KafkaTopic.label(),
            NodeKind::ExternalEndpoint.label(),
            NodeKind::Other.label(),
        ],
        edge_kinds: vec![
            EdgeKind::Contains.cypher_label(),
            EdgeKind::Calls.cypher_label(),
            EdgeKind::Extends.cypher_label(),
            EdgeKind::Implements.cypher_label(),
            EdgeKind::HasMethod.cypher_label(),
            EdgeKind::HasField.cypher_label(),
            EdgeKind::Imports.cypher_label(),
            EdgeKind::Accesses.cypher_label(),
            EdgeKind::Uses.cypher_label(),
            EdgeKind::MethodOverrides.cypher_label(),
            EdgeKind::MethodImplements.cypher_label(),
            EdgeKind::MemberOf.cypher_label(),
            EdgeKind::StepInProcess.cypher_label(),
            EdgeKind::HandlesRoute.cypher_label(),
            EdgeKind::PublishesEvent.cypher_label(),
            EdgeKind::ListensTo.cypher_label(),
            EdgeKind::ExternalCall.cypher_label(),
            EdgeKind::Tests.cypher_label(),
            EdgeKind::Other.cypher_label(),
        ],
    };
    serde_json::to_string_pretty(&schema).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        annotated_resource, paginate_resources, parse_page_query, read_community_nodes,
        render_resource_page, scan_jsonl_candidates, serialize_resource_page,
        serialized_resource_result_len, split_wiki_uri, PageQuery, ResourceCursor,
        RESOURCE_PAGE_SIZE,
    };
    use crate::domain::repository::ResolvedRepo;
    use cih_core::NodeKind;
    use std::io::Write;

    fn sample(n: usize) -> Vec<rmcp::model::Resource> {
        (0..n)
            .map(|i| annotated_resource(&format!("cih://r/{i}"), &format!("r{i}"), "d"))
            .collect()
    }

    /// nodes.jsonl with `n` Community records plus one non-matching Method row.
    fn write_fixture(dir: &std::path::Path, n: usize) -> std::path::PathBuf {
        let path = dir.join("nodes.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 0..n {
            writeln!(
                f,
                r#"{{"id":"Community:c{i}","kind":"Community","name":"c{i}"}}"#
            )
            .unwrap();
        }
        writeln!(f, r#"{{"id":"Method:m","kind":"Method","name":"m"}}"#).unwrap();
        path
    }

    fn entry_with_dir(dir: &std::path::Path) -> cih_core::RegistryEntry {
        cih_core::RegistryEntry {
            name: "r".into(),
            path: String::new(),
            graph_key: "r".into(),
            artifacts_dir: String::new(),
            community_artifacts_dir: Some(dir.display().to_string()),
            indexed_at: String::new(),
            last_git_head: None,
            stats: Default::default(),
        }
    }

    #[test]
    fn jsonl_scan_respects_offset_and_candidate_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), 5);
        let p1 = scan_jsonl_candidates(&path, "Community", 0, 2).unwrap();
        assert_eq!(p1.items.len(), 2);
        let p2 = scan_jsonl_candidates(&path, "Community", 2, 2).unwrap();
        assert_eq!(p2.items[0]["id"], "Community:c2");
        let p3 = scan_jsonl_candidates(&path, "Community", 4, 2).unwrap();
        assert_eq!(p3.items.len(), 1);
        // Non-matching kinds are invisible to paging.
        let all = scan_jsonl_candidates(&path, "Community", 0, 500).unwrap();
        assert_eq!(all.items.len(), 5);
    }

    #[test]
    fn serialized_resource_page_never_exceeds_its_byte_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_fixture(tmp.path(), 5);
        let entry = entry_with_dir(tmp.path());
        let candidates = scan_jsonl_candidates(&path, "Community", 0, 5).unwrap();
        let one = serialize_resource_page(
            &entry,
            "communities",
            "deadbeef",
            0,
            5,
            &candidates.items[..1],
            candidates.items.len(),
        )
        .unwrap();
        let uri = "cih://repo/r/communities";
        let budget = serialized_resource_result_len(&one, uri).unwrap();
        let page = render_resource_page(
            &entry,
            "communities",
            "deadbeef",
            0,
            5,
            &candidates.items,
            uri,
            budget,
        )
        .unwrap();
        assert!(serialized_resource_result_len(&page, uri).unwrap() <= budget);
        let body: serde_json::Value = serde_json::from_str(&page).unwrap();
        assert_eq!(body["count"], 1);
        assert_eq!(body["truncated_by"], "bytes");

        let err = render_resource_page(
            &entry,
            "communities",
            "deadbeef",
            0,
            5,
            &candidates.items,
            uri,
            budget - 1,
        )
        .unwrap_err();
        assert!(err.message.contains("first communities record"));
    }

    #[test]
    fn page_query_parses_and_rejects_unknowns() {
        let q = parse_page_query(Some("cursor=v:2&limit=10")).unwrap();
        assert_eq!(q.cursor.as_deref(), Some("v:2"));
        assert_eq!(q.limit, Some(10));
        assert!(parse_page_query(None).unwrap().cursor.is_none());
        assert!(parse_page_query(Some("limit=0")).is_err());
        assert!(parse_page_query(Some("limit=abc")).is_err());
        assert!(parse_page_query(Some("bogus=1")).is_err());
    }

    #[test]
    fn community_pages_stamp_and_validate_the_artifact_version() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("deadbeef"); // versioned-dir basename = cursor stamp
        std::fs::create_dir(&dir).unwrap();
        write_fixture(&dir, 3);
        let repo = ResolvedRepo::from_entry(entry_with_dir(&dir));
        let page = |cursor: Option<&str>, limit| PageQuery {
            cursor: cursor.map(str::to_string),
            limit,
        };

        // First page mints a version-stamped cursor and a ready-to-use next_uri.
        let text = read_community_nodes(
            &repo,
            NodeKind::Community,
            "communities",
            "cih://repo/r/communities?limit=2",
            &page(None, Some(2)),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["count"], 2);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["next_cursor"], "v1:deadbeef:communities:2");
        assert!(v["next_uri"]
            .as_str()
            .unwrap()
            .contains("communities?cursor=v1:deadbeef:communities:2"));

        // The minted cursor pages on to the final page.
        let text = read_community_nodes(
            &repo,
            NodeKind::Community,
            "communities",
            "cih://repo/r/communities?cursor=v1:deadbeef:communities:2&limit=2",
            &page(Some("v1:deadbeef:communities:2"), Some(2)),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["count"], 1);
        assert_eq!(v["next_cursor"], serde_json::Value::Null);
        assert_eq!(v["truncated"], false);

        // A cursor from a different artifact version fails loudly.
        let err = read_community_nodes(
            &repo,
            NodeKind::Community,
            "communities",
            "cih://repo/r/communities?cursor=v1:cafebabe:communities:2",
            &page(Some("v1:cafebabe:communities:2"), None),
        )
        .unwrap_err();
        assert!(err.message.contains("stale cursor"), "{}", err.message);

        // A malformed cursor is invalid_params, not a silent first page.
        let err = read_community_nodes(
            &repo,
            NodeKind::Community,
            "communities",
            "cih://repo/r/communities?cursor=nonsense",
            &page(Some("nonsense"), None),
        )
        .unwrap_err();
        assert!(err.message.contains("malformed"), "{}", err.message);
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);

        // A typed cursor cannot be reused for a different resource kind.
        let err = read_community_nodes(
            &repo,
            NodeKind::Process,
            "processes",
            "cih://repo/r/processes?cursor=v1:deadbeef:communities:2",
            &page(Some("v1:deadbeef:communities:2"), None),
        )
        .unwrap_err();
        assert!(err.message.contains("cursor is for 'communities'"));
    }

    #[test]
    fn resource_cursor_round_trips_all_identity_fields() {
        let cursor = ResourceCursor {
            version: "deadbeef".into(),
            section: "processes".into(),
            offset: 42,
        };
        assert_eq!(ResourceCursor::decode(&cursor.encode()).unwrap(), cursor);
    }

    #[test]
    fn first_page_caps_at_page_size_and_sets_next_cursor() {
        let res = paginate_resources(sample(250), None).unwrap();
        assert_eq!(res.resources.len(), RESOURCE_PAGE_SIZE);
        assert_eq!(res.next_cursor.as_deref(), Some("100"));
    }

    #[test]
    fn cursor_advances_through_every_page_once() {
        let p1 = paginate_resources(sample(250), None).unwrap();
        let p2 = paginate_resources(sample(250), p1.next_cursor).unwrap();
        assert_eq!(p2.resources.len(), 100);
        assert_eq!(p2.next_cursor.as_deref(), Some("200"));
        let p3 = paginate_resources(sample(250), p2.next_cursor).unwrap();
        assert_eq!(p3.resources.len(), 50);
        assert_eq!(p3.next_cursor, None, "last page has no next cursor");
    }

    #[test]
    fn small_list_returns_one_page_without_cursor() {
        let res = paginate_resources(sample(5), None).unwrap();
        assert_eq!(res.resources.len(), 5);
        assert_eq!(res.next_cursor, None);
    }

    #[test]
    fn invalid_cursor_is_invalid_params() {
        let err = paginate_resources(sample(10), Some("not-a-number".into())).unwrap_err();
        assert_eq!(err.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    }

    #[test]
    fn wiki_uri_splits_on_first_wiki_segment() {
        assert_eq!(
            split_wiki_uri("fineract/wiki/fineract-provider/dev/loan-x"),
            Some(("fineract", "fineract-provider/dev/loan-x"))
        );
        // A slug containing "wiki" still splits on the first occurrence.
        assert_eq!(
            split_wiki_uri("repo1/wiki/docs/wiki/setup"),
            Some(("repo1", "docs/wiki/setup"))
        );
    }

    #[test]
    fn non_wiki_uris_fall_through() {
        assert_eq!(split_wiki_uri("repo1/context"), None);
        assert_eq!(split_wiki_uri("repo1/wiki/"), None);
        assert_eq!(split_wiki_uri("/wiki/slug"), None);
    }
}
