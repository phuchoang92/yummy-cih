use cih_core::{EdgeKind, NodeKind, Registry};
use rmcp::{
    model::{
        AnnotateAble, ListResourcesResult, ListResourceTemplatesResult, PaginatedRequestParam,
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

/// Build the static list of resources from the registry.
pub fn list_resources(
    _request: Option<PaginatedRequestParam>,
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
    Ok(ListResourcesResult::with_all_items(resources))
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
    ];
    Ok(ListResourceTemplatesResult::with_all_items(templates))
}

/// Serve one resource by URI.
pub fn read_resource(
    request: ReadResourceRequestParam,
) -> Result<ReadResourceResult, McpError> {
    let uri = &request.uri;

    // Parse cih://repo/{name}/{section}
    let rest = uri
        .strip_prefix("cih://repo/")
        .ok_or_else(|| McpError::invalid_params(format!("unknown URI scheme: {uri}"), None))?;
    let (name, section) = rest
        .rsplit_once('/')
        .ok_or_else(|| McpError::invalid_params(format!("malformed CIH URI: {uri}"), None))?;

    let reg = Registry::load();
    let entry = reg
        .find(name)
        .ok_or_else(|| McpError::invalid_params(format!("repo '{name}' not in registry"), None))?;

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
            ))
        }
    };

    Ok(ReadResourceResult {
        contents: vec![ResourceContents::text(text, uri)],
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
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| McpError::internal_error(format!("cannot read {}: {e}", path.display()), None))?;
    let label = kind.label();
    let nodes: Vec<serde_json::Value> = raw
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &serde_json::Value| v.get("kind").and_then(|k| k.as_str()) == Some(label))
        .collect();
    serde_json::to_string_pretty(&nodes)
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
            EdgeKind::Other.cypher_label(),
        ],
    };
    serde_json::to_string_pretty(&schema).unwrap_or_default()
}
