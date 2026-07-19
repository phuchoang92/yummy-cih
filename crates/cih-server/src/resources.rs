use cih_core::{EdgeKind, NodeKind, Registry};
use rmcp::{
    model::{
        AnnotateAble, ListResourceTemplatesResult, ListResourcesResult, PaginatedRequestParam,
        RawResource, RawResourceTemplate, ReadResourceRequestParam, ReadResourceResult,
        ResourceContents,
    },
    ErrorData as McpError,
};

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
    request: Option<PaginatedRequestParam>,
) -> Result<ListResourcesResult, McpError> {
    let reg = Registry::load();
    let mut resources = Vec::new();
    for entry in &reg.entries {
        let n = &entry.name;
        resources.push(annotated_resource(
            &format!("cih://repo/{n}/context"),
            &format!("{n}/context"),
            &format!("Registry entry and stats for repo '{n}'"),
        ));
        resources.push(annotated_resource(
            &format!("cih://repo/{n}/communities"),
            &format!("{n}/communities"),
            &format!("Community cluster nodes for repo '{n}'"),
        ));
        resources.push(annotated_resource(
            &format!("cih://repo/{n}/processes"),
            &format!("{n}/processes"),
            &format!("Process trace nodes for repo '{n}'"),
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
            description: Some("Community cluster nodes for an indexed repo".to_string()),
            mime_type: Some("application/json".to_string()),
        }
        .no_annotation(),
        RawResourceTemplate {
            uri_template: "cih://repo/{name}/processes".to_string(),
            name: "repo-processes".to_string(),
            title: Some("Repo processes".to_string()),
            description: Some("Process trace nodes for an indexed repo".to_string()),
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
pub fn read_resource(request: ReadResourceRequestParam) -> Result<ReadResourceResult, McpError> {
    let uri = &request.uri;

    // Parse cih://repo/{name}/{section}
    let rest = uri
        .strip_prefix("cih://repo/")
        .ok_or_else(|| McpError::invalid_params(format!("unknown URI scheme: {uri}"), None))?;

    // Wiki page URIs must be handled before the generic name/section split:
    // slugs contain slashes, which rsplit_once would misattribute to the name.
    if let Some((name, slug)) = split_wiki_uri(rest) {
        return read_wiki_page(name, slug, uri);
    }

    let (name, section) = rest
        .rsplit_once('/')
        .ok_or_else(|| McpError::invalid_params(format!("malformed CIH URI: {uri}"), None))?;

    let reg = Registry::load();
    let entry = reg
        .find(name)
        .ok_or_else(|| crate::utils::repo_not_found(name))?;

    let text = match section {
        "context" => serde_json::to_string_pretty(entry)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?,
        "communities" => read_community_nodes(entry, NodeKind::Community)?,
        "processes" => read_community_nodes(entry, NodeKind::Process)?,
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

/// Split `"{name}/wiki/{slug}"` on the FIRST `/wiki/` into `(name, slug)`.
/// Registry repo names never contain slashes, so first-occurrence is correct
/// even when the slug itself contains a `wiki` segment.
pub fn split_wiki_uri(rest: &str) -> Option<(&str, &str)> {
    rest.split_once("/wiki/")
        .filter(|(name, slug)| !name.is_empty() && !slug.is_empty())
}

/// Serve one wiki page as markdown. Uncached like the other resource reads:
/// registry → `<repo>/.cih/wiki/manifest.json` → slug lookup → page file.
fn read_wiki_page(name: &str, slug: &str, uri: &str) -> Result<ReadResourceResult, McpError> {
    let reg = Registry::load();
    let entry = reg
        .find(name)
        .ok_or_else(|| crate::utils::repo_not_found(name))?;
    let wiki_dir = std::path::Path::new(&entry.path).join(".cih").join("wiki");
    let manifest_raw = std::fs::read_to_string(wiki_dir.join("manifest.json")).map_err(|_| {
        McpError::invalid_params(
            format!("no generated wiki for '{name}' — run `cih-engine wiki` first"),
            None,
        )
    })?;
    let manifest: crate::wiki::Manifest = serde_json::from_str(&manifest_raw)
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
    let markdown = crate::wiki::read_page_raw(&wiki_dir, &page.path).ok_or_else(|| {
        McpError::internal_error(
            format!("wiki page '{slug}' exists in the manifest but its file is unreadable"),
            None,
        )
    })?;
    Ok(ReadResourceResult {
        contents: vec![ResourceContents::text(markdown, uri)],
    })
}

fn read_community_nodes(
    entry: &cih_core::RegistryEntry,
    kind: NodeKind,
) -> Result<String, McpError> {
    let dir = entry
        .community_artifacts_dir
        .as_deref()
        .ok_or_else(|| McpError::invalid_params("discover not run for this repo yet", None))?;
    let path = std::path::Path::new(dir).join("nodes.jsonl");
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        McpError::internal_error(format!("cannot read {}: {e}", path.display()), None)
    })?;
    let label = kind.label();
    let nodes: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &serde_json::Value| v.get("kind").and_then(|k| k.as_str()) == Some(label))
        .collect();
    serde_json::to_string_pretty(&nodes).map_err(|e| McpError::internal_error(e.to_string(), None))
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
    use super::{annotated_resource, paginate_resources, split_wiki_uri, RESOURCE_PAGE_SIZE};

    fn sample(n: usize) -> Vec<rmcp::model::Resource> {
        (0..n)
            .map(|i| annotated_resource(&format!("cih://r/{i}"), &format!("r{i}"), "d"))
            .collect()
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
