//! Phase 3 parse driver: selected Java files -> structure graph + unresolved IR.
//!
//! This crate intentionally stops before Phase 4 resolution. It emits stable
//! structure nodes/edges and persists `ParsedFile`s containing raw imports and
//! unresolved reference sites for the next phase.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cih_core::{file_id, folder_id, Edge, EdgeKind, Node, NodeId, ParsedFile, Range};
use rayon::prelude::*;

mod java;

#[derive(Clone, Debug, Default)]
pub struct ParseOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub parsed_files: Vec<ParsedFile>,
    /// Files that could not be read/parsed. The rest of the run still succeeds —
    /// on a large repo one bad file must not abort indexing.
    pub skipped: Vec<SkippedFile>,
}

#[derive(Clone, Debug)]
pub struct SkippedFile {
    pub rel: String,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub struct ParseArtifacts {
    pub parsed_files_path: PathBuf,
}

#[derive(Clone, Debug)]
struct ParseUnit {
    rel: String,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    parsed_file: ParsedFile,
}

pub fn parse_files(repo_root: &Path, files: &[String]) -> Result<ParseOutput> {
    // Per-file failures are collected, not propagated: one unreadable/garbage file
    // must not abort indexing of a 12k-file repo.
    let results = files
        .par_iter()
        .map(|rel| (rel.clone(), parse_one(repo_root, rel)))
        .collect::<Vec<_>>();

    let mut units = Vec::new();
    let mut skipped = Vec::new();
    for (rel, result) in results {
        match result {
            Ok(unit) => units.push(unit),
            Err(err) => skipped.push(SkippedFile {
                rel,
                reason: format!("{err:#}"),
            }),
        }
    }
    units.sort_by(|a, b| a.rel.cmp(&b.rel));
    skipped.sort_by(|a, b| a.rel.cmp(&b.rel));

    let mut nodes = BTreeMap::new();
    let mut edges = BTreeMap::new();
    let mut parsed_files = Vec::new();

    for unit in units {
        add_structure_path(&unit.rel, &mut nodes, &mut edges);
        for node in unit.nodes {
            insert_node(&mut nodes, node);
        }
        for edge in unit.edges {
            insert_edge(&mut edges, edge);
        }
        parsed_files.push(unit.parsed_file);
    }

    Ok(ParseOutput {
        nodes: nodes.into_values().collect(),
        edges: edges.into_values().collect(),
        parsed_files,
        skipped,
    })
}

pub fn write_parsed_files(dir: &Path, parsed_files: &[ParsedFile]) -> Result<ParseArtifacts> {
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let parsed_files_path = dir.join("parsed-files.jsonl");
    let mut writer = BufWriter::new(
        File::create(&parsed_files_path)
            .with_context(|| format!("failed to create {}", parsed_files_path.display()))?,
    );
    for parsed in parsed_files {
        serde_json::to_writer(&mut writer, parsed)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;

    Ok(ParseArtifacts { parsed_files_path })
}

fn parse_one(repo_root: &Path, rel: &str) -> Result<ParseUnit> {
    let path = repo_root.join(rel);
    let src =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    // The File node + its Folder ancestry + CONTAINS edges are emitted centrally by
    // `add_structure_path` during merge, so the parse unit only carries declarations.
    java::parse_java_file(rel, &src)
}

fn add_structure_path(
    rel: &str,
    nodes: &mut BTreeMap<String, Node>,
    edges: &mut BTreeMap<(String, String, &'static str), Edge>,
) {
    let parts = rel
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let mut current = String::new();
    let mut parent: Option<NodeId> = None;

    for (index, part) in parts.iter().enumerate() {
        if current.is_empty() {
            current.push_str(part);
        } else {
            current.push('/');
            current.push_str(part);
        }

        let is_file = index + 1 == parts.len();
        let id = if is_file {
            file_id(&current)
        } else {
            folder_id(&current)
        };
        let kind = if is_file {
            cih_core::NodeKind::File
        } else {
            cih_core::NodeKind::Folder
        };
        insert_node(
            nodes,
            Node {
                id: id.clone(),
                kind,
                name: (*part).to_string(),
                qualified_name: None,
                file: current.clone(),
                range: Range::default(),
                props: Some(serde_json::json!({ "filePath": current })),
            },
        );

        if let Some(parent_id) = parent {
            insert_edge(
                edges,
                Edge {
                    src: parent_id,
                    dst: id.clone(),
                    kind: EdgeKind::Contains,
                    confidence: 1.0,
                    reason: "structure".into(),
                },
            );
        }
        parent = Some(id);
    }
}

fn insert_node(nodes: &mut BTreeMap<String, Node>, node: Node) {
    nodes.entry(node.id.as_str().to_string()).or_insert(node);
}

fn insert_edge(edges: &mut BTreeMap<(String, String, &'static str), Edge>, edge: Edge) {
    edges
        .entry((
            edge.src.as_str().to_string(),
            edge.dst.as_str().to_string(),
            edge.kind.cypher_label(),
        ))
        .or_insert(edge);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{constructor_id, field_id, method_id, type_id, RefKind};

    fn temp_repo() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("cih-parse-test-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn parses_java_structure_ir_and_references() {
        let root = temp_repo();
        let rel = "src/main/java/com/example/OwnerController.java";
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"
package com.example;

import java.util.List;
import static com.example.Util.*;

class Base {}
interface Handler {}

@RestController
@RequestMapping(path = "/owners")
class OwnerController extends Base implements Handler {
    private OwnerService service;

    public OwnerController(OwnerService service) {
        this.service = service;
    }

    @GetMapping("/{id}")
    public Owner findOwner(Long id) {
        return service.findOwner(id);
    }

    @PostMapping(path = "/search", produces = "application/json")
    public void search() {
        service.findOwner(1L);
    }

    class Inner {
        void ping() {
            helper();
        }
    }
}
"#,
        )
        .unwrap();

        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let parsed = output.parsed_files.first().unwrap();
        assert_eq!(parsed.package.as_deref(), Some("com.example"));
        assert!(parsed.imports.iter().any(|imp| imp.raw == "java.util.List"));
        assert!(parsed
            .imports
            .iter()
            .any(|imp| imp.raw == "com.example.Util.*" && imp.is_static && imp.is_wildcard));

        assert!(parsed.defs.iter().any(|def| {
            def.kind == cih_core::NodeKind::Class && def.fqcn == "com.example.OwnerController"
        }));
        assert!(parsed.defs.iter().any(|def| {
            def.kind == cih_core::NodeKind::Class
                && def.fqcn == "com.example.OwnerController.Inner"
                && def.owner
                    == Some(type_id(
                        cih_core::NodeKind::Class,
                        "com.example.OwnerController",
                    ))
        }));
        assert!(parsed.defs.iter().any(|def| {
            def.kind == cih_core::NodeKind::Method
                && def.name == "findOwner"
                && def.id == method_id("com.example.OwnerController", "findOwner", 1)
        }));
        assert!(parsed.defs.iter().any(|def| {
            def.kind == cih_core::NodeKind::Constructor
                && def.id == constructor_id("com.example.OwnerController", 1)
        }));
        assert!(parsed.defs.iter().any(|def| {
            def.kind == cih_core::NodeKind::Field
                && def.id == field_id("com.example.OwnerController", "service")
        }));

        assert!(parsed.reference_sites.iter().any(|site| {
            site.kind == RefKind::Call
                && site.name == "findOwner"
                && site.receiver.as_deref() == Some("service")
                && site.arity == Some(1)
                && site.in_fqcn == "com.example.OwnerController#findOwner/1"
        }));
        assert!(parsed
            .reference_sites
            .iter()
            .any(|site| site.kind == RefKind::Extends && site.name == "Base"));
        assert!(parsed
            .reference_sites
            .iter()
            .any(|site| site.kind == RefKind::Implements && site.name == "Handler"));

        assert!(output
            .nodes
            .iter()
            .any(|node| node.id == file_id(rel) && node.kind == cih_core::NodeKind::File));
        let controller = output
            .nodes
            .iter()
            .find(|node| {
                node.id == type_id(cih_core::NodeKind::Class, "com.example.OwnerController")
            })
            .unwrap();
        assert_eq!(
            controller
                .props
                .as_ref()
                .and_then(|props| props.get("stereotype"))
                .and_then(|value| value.as_str()),
            Some("controller")
        );
        assert!(output.edges.iter().any(|edge| {
            edge.kind == EdgeKind::HasMethod
                && edge.src == type_id(cih_core::NodeKind::Class, "com.example.OwnerController")
                && edge.dst == method_id("com.example.OwnerController", "findOwner", 1)
        }));
        assert!(output.edges.iter().any(|edge| {
            edge.kind == EdgeKind::Contains
                && edge.src == type_id(cih_core::NodeKind::Class, "com.example.OwnerController")
                && edge.dst
                    == type_id(
                        cih_core::NodeKind::Class,
                        "com.example.OwnerController.Inner",
                    )
        }));
        let route_id = cih_core::NodeId::new("Route:GET /owners/{id}");
        assert!(output.nodes.iter().any(|node| {
            node.id == route_id
                && node.kind == cih_core::NodeKind::Route
                && node
                    .props
                    .as_ref()
                    .and_then(|props| props.get("httpMethod"))
                    .and_then(|value| value.as_str())
                    == Some("GET")
        }));
        assert!(output.edges.iter().any(|edge| {
            edge.kind == EdgeKind::HandlesRoute
                && edge.src == method_id("com.example.OwnerController", "findOwner", 1)
                && edge.dst == route_id
        }));
        assert!(!output
            .nodes
            .iter()
            .any(|node| node.id.as_str() == "Route:POST /owners/application/json"));
    }

    #[test]
    fn unreadable_file_is_skipped_without_aborting() {
        let root = temp_repo();
        let good = "src/main/java/com/example/Ok.java";
        let good_path = root.join(good);
        fs::create_dir_all(good_path.parent().unwrap()).unwrap();
        fs::write(&good_path, "package com.example;\nclass Ok {}\n").unwrap();

        let missing = "src/main/java/com/example/Missing.java"; // never created on disk
        let output = parse_files(&root, &[good.to_string(), missing.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        // The good file parsed; the missing one was skipped, not fatal.
        assert_eq!(output.parsed_files.len(), 1);
        assert_eq!(output.parsed_files[0].file, good);
        assert_eq!(output.skipped.len(), 1);
        assert_eq!(output.skipped[0].rel, missing);
        assert!(output
            .nodes
            .iter()
            .any(|node| node.id == type_id(cih_core::NodeKind::Class, "com.example.Ok")));
    }

    #[test]
    fn explicit_receiver_parameter_excluded_from_arity() {
        let root = temp_repo();
        let rel = "src/main/java/com/example/Receiver.java";
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "package com.example;\nclass Receiver {\n  void touch(Receiver this, int x) {}\n}\n",
        )
        .unwrap();

        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        // Arity counts the single argument `int x`, not the explicit receiver.
        let parsed = output.parsed_files.first().unwrap();
        assert!(parsed.defs.iter().any(|def| {
            def.kind == cih_core::NodeKind::Method
                && def.id == method_id("com.example.Receiver", "touch", 1)
        }));
    }

    fn stereotype_of(output: &ParseOutput, fqcn: &str) -> Option<String> {
        output
            .nodes
            .iter()
            .find(|node| node.id == type_id(cih_core::NodeKind::Class, fqcn))
            .and_then(|node| node.props.as_ref())
            .and_then(|props| props.get("stereotype"))
            .and_then(|value| value.as_str())
            .map(str::to_string)
    }

    #[test]
    fn stereotype_uses_own_annotations_not_body() {
        let root = temp_repo();
        let rel = "src/main/java/com/example/Stereotypes.java";
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"
package com.example;
// A @Service whose body has a @GetMapping method must NOT be tagged a controller.
@Service
class FooService {
    @GetMapping("/x")
    public void m() {}
}
@Repository
class FooRepo {}
@Entity
class FooEntity {}
class Plain {}
"#,
        )
        .unwrap();

        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            stereotype_of(&output, "com.example.FooService").as_deref(),
            Some("service"),
            "a @Service with a @GetMapping body must stay a service"
        );
        assert_eq!(
            stereotype_of(&output, "com.example.FooRepo").as_deref(),
            Some("repository")
        );
        assert_eq!(
            stereotype_of(&output, "com.example.FooEntity").as_deref(),
            Some("entity")
        );
        assert_eq!(stereotype_of(&output, "com.example.Plain"), None);
    }

    #[test]
    fn array_form_mapping_yields_all_routes() {
        let root = temp_repo();
        let rel = "src/main/java/com/example/Multi.java";
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"
package com.example;
@RestController
class Multi {
    @GetMapping({"/a", "/b"})
    public void m() {}
}
"#,
        )
        .unwrap();

        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        for path in ["Route:GET /a", "Route:GET /b"] {
            assert!(
                output.nodes.iter().any(|node| node.id.as_str() == path),
                "expected route {path}"
            );
        }
    }
}
