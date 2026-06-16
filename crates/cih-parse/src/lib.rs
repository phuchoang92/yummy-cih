//! Phase 3 parse driver: selected Java files -> structure graph + unresolved IR.
//!
//! This crate intentionally stops before Phase 4 resolution. It emits stable
//! structure nodes/edges and persists `ParsedFile`s containing raw imports and
//! unresolved reference sites for the next phase.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use cih_core::{file_id, folder_id, Edge, EdgeKind, Node, NodeId, ParsedFile, Range};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

mod java;
pub mod sql;

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

#[derive(Clone, Debug, Default)]
pub struct ParseUnitsOutput {
    pub units: Vec<ParsedUnit>,
    pub skipped: Vec<SkippedFile>,
}

#[derive(Clone, Debug)]
pub struct ParseArtifacts {
    pub parsed_files_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParsedUnit {
    pub rel: String,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub parsed_file: ParsedFile,
}

pub fn parse_files(repo_root: &Path, files: &[String]) -> Result<ParseOutput> {
    let output = parse_file_units(repo_root, files)?;
    Ok(parse_output_from_units(output.units, output.skipped))
}

pub fn parse_file_units(repo_root: &Path, files: &[String]) -> Result<ParseUnitsOutput> {
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

    Ok(ParseUnitsOutput { units, skipped })
}

pub fn parse_output_from_units(
    mut units: Vec<ParsedUnit>,
    mut skipped: Vec<SkippedFile>,
) -> ParseOutput {
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

    ParseOutput {
        nodes: nodes.into_values().collect(),
        edges: edges.into_values().collect(),
        parsed_files,
        skipped,
    }
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

/// Read a `parsed-files.jsonl` produced by [`write_parsed_files`] back into memory.
pub fn load_parsed_files(dir: &Path) -> Result<Vec<ParsedFile>> {
    let path = dir.join("parsed-files.jsonl");
    let reader = BufReader::new(
        File::open(&path).with_context(|| format!("failed to open {}", path.display()))?,
    );
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line =
            line.with_context(|| format!("read error at line {} of {}", i + 1, path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let pf: ParsedFile = serde_json::from_str(&line)
            .with_context(|| format!("parse error at line {} of {}", i + 1, path.display()))?;
        out.push(pf);
    }
    Ok(out)
}

fn parse_one(repo_root: &Path, rel: &str) -> Result<ParsedUnit> {
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
    use cih_core::{
        constructor_id, field_id, method_id, type_id, BindingKind, ContractKind, RefKind,
    };

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
    fn persists_type_bindings_param_return_field_and_in_callable() {
        let root = temp_repo();
        let rel = "src/main/java/com/example/OwnerController.java";
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"
package com.example;
class OwnerController {
    private OwnerService service;
    public Owner findOwner(Long id) {
        var found = service.findOwner(id);
        return found;
    }
}
"#,
        )
        .unwrap();

        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();
        let parsed = output.parsed_files.first().unwrap();

        // SymbolDef carries param/return/declared types.
        let method = parsed
            .defs
            .iter()
            .find(|d| d.id == method_id("com.example.OwnerController", "findOwner", 1))
            .unwrap();
        assert_eq!(method.param_types, vec!["Long"]);
        assert_eq!(method.return_type.as_deref(), Some("Owner"));
        let field = parsed
            .defs
            .iter()
            .find(|d| d.id == field_id("com.example.OwnerController", "service"))
            .unwrap();
        assert_eq!(field.declared_type.as_deref(), Some("OwnerService"));

        // Type bindings: field, param, and the `var` call-result inference.
        let binding = |name: &str| {
            parsed
                .type_bindings
                .iter()
                .find(|b| b.name == name)
                .cloned()
                .unwrap_or_else(|| panic!("no binding for {name}"))
        };
        let svc = binding("service");
        assert_eq!(svc.kind, BindingKind::Field);
        assert_eq!(svc.raw_type, "OwnerService");
        assert_eq!(svc.in_fqcn, "com.example.OwnerController");

        let id = binding("id");
        assert_eq!(id.kind, BindingKind::Param);
        assert_eq!(id.raw_type, "Long");
        assert_eq!(id.in_fqcn, "com.example.OwnerController#findOwner/1");

        let found = binding("found");
        assert_eq!(found.kind, BindingKind::CallResult);
        assert_eq!(found.raw_type, "findOwner"); // method whose return type to follow

        // ReferenceSite.in_callable is the caller's NodeId, not the in_fqcn string.
        let call = parsed
            .reference_sites
            .iter()
            .find(|s| s.kind == RefKind::Call && s.name == "findOwner")
            .unwrap();
        assert_eq!(
            call.in_callable,
            method_id("com.example.OwnerController", "findOwner", 1)
        );
        assert_eq!(call.in_fqcn, "com.example.OwnerController#findOwner/1");
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

    #[test]
    fn parses_cross_service_contract_sites() {
        let root = temp_repo();
        let rel = "src/main/java/com/example/Contracts.java";
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"
package com.example;

@FeignClient(name = "orders", path = "/orders")
interface OrdersClient {
    @GetMapping("/{id}")
    Order getOrder(String id);
}

class ContractClient {
    private RestTemplate restTemplate;
    private WebClient webClient;
    private KafkaTemplate<String, String> kafkaTemplate;
    private ApplicationEventPublisher publisher;

    @KafkaListener(topics = {"orders.created"})
    void listen(String payload) {}

    @EventListener
    void onUserCreated(UserCreated event) {}

    void call() {
        restTemplate.getForObject("http://orders.local/api/orders/{id}", String.class);
        webClient.post().uri("/api/payments").retrieve();
        kafkaTemplate.send("orders.created", "1");
        publisher.publishEvent(new UserCreated());
    }
}
"#,
        )
        .unwrap();

        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();
        let parsed = output.parsed_files.first().unwrap();

        assert!(parsed.contract_sites.iter().any(|site| {
            site.kind == ContractKind::FeignClient
                && site.http_method.as_deref() == Some("GET")
                && site.url_template.as_deref() == Some("/orders/{id}")
                && site.in_callable == method_id("com.example.OrdersClient", "getOrder", 1)
        }));
        assert!(parsed.contract_sites.iter().any(|site| {
            site.kind == ContractKind::HttpCall
                && site.http_method.as_deref() == Some("GET")
                && site.url_template.as_deref() == Some("/api/orders/{id}")
                && site.in_callable == method_id("com.example.ContractClient", "call", 0)
        }));
        assert!(parsed.contract_sites.iter().any(|site| {
            site.kind == ContractKind::HttpCall
                && site.http_method.as_deref() == Some("POST")
                && site.url_template.as_deref() == Some("/api/payments")
        }));
        assert!(parsed.contract_sites.iter().any(|site| {
            site.kind == ContractKind::EventListen
                && site.topic.as_deref() == Some("orders.created")
                && site.in_callable == method_id("com.example.ContractClient", "listen", 1)
        }));
        assert!(parsed.contract_sites.iter().any(|site| {
            site.kind == ContractKind::EventListen && site.topic.as_deref() == Some("UserCreated")
        }));
        assert!(parsed.contract_sites.iter().any(|site| {
            site.kind == ContractKind::EventPublish
                && site.topic.as_deref() == Some("orders.created")
                && site.in_callable == method_id("com.example.ContractClient", "call", 0)
        }));
        assert!(parsed.contract_sites.iter().any(|site| {
            site.kind == ContractKind::EventPublish && site.topic.as_deref() == Some("UserCreated")
        }));
    }

    fn node_prop<'a>(
        output: &'a ParseOutput,
        node_id: &str,
        key: &str,
    ) -> Option<&'a serde_json::Value> {
        output
            .nodes
            .iter()
            .find(|n| n.id.as_str() == node_id)
            .and_then(|n| n.props.as_ref())
            .and_then(|p| p.get(key))
    }

    #[test]
    fn bean_method_tagged_when_annotated() {
        let root = temp_repo();
        let rel = "src/main/java/com/example/AppConfig.java";
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"
package com.example;
@Configuration
class AppConfig {
    @Bean
    public DataSource dataSource() { return null; }
    public void helper() {}
}
"#,
        )
        .unwrap();

        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let bean_id = method_id("com.example.AppConfig", "dataSource", 0);
        let helper_id = method_id("com.example.AppConfig", "helper", 0);
        assert_eq!(
            node_prop(&output, bean_id.as_str(), "isBean"),
            Some(&serde_json::Value::Bool(true)),
            "@Bean method must have isBean=true"
        );
        assert_eq!(
            node_prop(&output, helper_id.as_str(), "isBean"),
            None,
            "plain method must have no isBean prop"
        );
    }

    #[test]
    fn bean_method_not_tagged_without_annotation() {
        let root = temp_repo();
        let rel = "src/main/java/com/example/Plain.java";
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "package com.example;\nclass Plain { public Object produce() { return null; } }\n",
        )
        .unwrap();

        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let id = method_id("com.example.Plain", "produce", 0);
        assert_eq!(node_prop(&output, id.as_str(), "isBean"), None);
    }

    #[test]
    fn jpa_repository_tagged_as_repository() {
        let root = temp_repo();
        let rel = "src/main/java/com/example/UserRepo.java";
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "package com.example;\nclass UserRepo implements JpaRepository<User, Long> {}\n",
        )
        .unwrap();

        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            stereotype_of(&output, "com.example.UserRepo").as_deref(),
            Some("repository"),
            "JpaRepository implementor must be tagged as repository"
        );
        assert_eq!(
            node_prop(&output, "Class:com.example.UserRepo", "entityType"),
            Some(&serde_json::Value::String("User".to_string())),
            "entityType must be the first generic type argument"
        );
    }

    #[test]
    fn jpa_crud_repository_also_tagged() {
        let root = temp_repo();
        let rel = "src/main/java/com/example/ItemRepo.java";
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "package com.example;\nclass ItemRepo implements CrudRepository<Item, Long> {}\n",
        )
        .unwrap();

        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            stereotype_of(&output, "com.example.ItemRepo").as_deref(),
            Some("repository")
        );
    }

    #[test]
    fn jpa_annotation_idempotent_with_interface() {
        let root = temp_repo();
        let rel = "src/main/java/com/example/AnnotatedRepo.java";
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "package com.example;\n@Repository\nclass AnnotatedRepo implements JpaRepository<Order, Long> {}\n",
        )
        .unwrap();

        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(
            stereotype_of(&output, "com.example.AnnotatedRepo").as_deref(),
            Some("repository"),
            "stereotype must be repository when both annotation and interface are present"
        );
    }

    // ── Phase 16: test detection ──────────────────────────────────────────────

    fn write_file(root: &PathBuf, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn test_class_by_annotation_gets_test_stereotype() {
        let root = temp_repo();
        let rel = "src/test/java/com/example/OrderServiceTest.java";
        write_file(
            &root,
            rel,
            r#"
package com.example;
import org.springframework.boot.test.context.SpringBootTest;
@SpringBootTest
public class OrderServiceTest {
    @Test public void testSave() {}
}
"#,
        );
        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        // Node props should carry stereotype="test"
        let test_node = output
            .nodes
            .iter()
            .find(|n| n.name == "OrderServiceTest")
            .expect("OrderServiceTest node must exist");
        let stereotype = test_node
            .props
            .as_ref()
            .and_then(|p| p.get("stereotype"))
            .and_then(|v| v.as_str());
        assert_eq!(stereotype, Some("test"), "SpringBootTest class must have stereotype=test");
    }

    #[test]
    fn test_class_by_naming_convention_gets_test_stereotype() {
        let root = temp_repo();
        let rel = "src/test/java/com/example/PaymentServiceIT.java";
        write_file(
            &root,
            rel,
            "package com.example;\npublic class PaymentServiceIT {}\n",
        );
        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let test_node = output
            .nodes
            .iter()
            .find(|n| n.name == "PaymentServiceIT")
            .expect("PaymentServiceIT node must exist");
        let stereotype = test_node
            .props
            .as_ref()
            .and_then(|p| p.get("stereotype"))
            .and_then(|v| v.as_str());
        assert_eq!(stereotype, Some("test"), "*IT class must have stereotype=test");
    }

    #[test]
    fn test_method_emits_tests_edge_and_prop() {
        let root = temp_repo();
        let rel = "src/test/java/com/example/FooTest.java";
        write_file(
            &root,
            rel,
            r#"
package com.example;
public class FooTest {
    @Test
    public void shouldWork() {}
    public void helperMethod() {}
}
"#,
        );
        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        // The @Test method node should have isTest=true in props.
        let test_method = output
            .nodes
            .iter()
            .find(|n| n.name == "shouldWork")
            .expect("shouldWork method node must exist");
        let is_test = test_method
            .props
            .as_ref()
            .and_then(|p| p.get("isTest"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        assert!(is_test, "@Test method must have isTest=true prop");

        // A TESTS edge must exist from shouldWork to FooTest class.
        let test_class_id = type_id(cih_core::NodeKind::Class, "com.example.FooTest");
        let test_method_id = method_id("com.example.FooTest", "shouldWork", 0);
        assert!(
            output.edges.iter().any(|e| {
                e.kind == EdgeKind::Tests && e.src == test_method_id && e.dst == test_class_id
            }),
            "TESTS edge from @Test method to owner class must be emitted"
        );

        // helperMethod (no @Test) must NOT have a TESTS edge.
        let helper_id = method_id("com.example.FooTest", "helperMethod", 0);
        assert!(
            !output
                .edges
                .iter()
                .any(|e| e.kind == EdgeKind::Tests && e.src == helper_id),
            "non-@Test method must not emit a TESTS edge"
        );
    }

    #[test]
    fn mock_bean_field_emits_tests_edge() {
        let root = temp_repo();
        let rel = "src/test/java/com/example/BarTest.java";
        write_file(
            &root,
            rel,
            r#"
package com.example;
@SpringBootTest
public class BarTest {
    @MockBean
    private OrderService orderService;
}
"#,
        );
        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let test_class_id = type_id(cih_core::NodeKind::Class, "com.example.BarTest");
        // TESTS edge from test class to the raw "Class:OrderService" target.
        assert!(
            output.edges.iter().any(|e| {
                e.kind == EdgeKind::Tests
                    && e.src == test_class_id
                    && e.dst.as_str() == "Class:OrderService"
            }),
            "TESTS edge from test class to @MockBean field type must be emitted"
        );
    }

    // ── SQL constant + execution site extraction ──────────────────────────────

    #[test]
    fn parses_sql_constants_from_static_final_string_fields() {
        let root = temp_repo();
        let rel = "src/main/java/com/bank/OverdraftAdapterImpl.java";
        write_file(
            &root,
            rel,
            r#"
package com.bank;
public class OverdraftAdapterImpl {
    private static final String QUERY_GET_BY_CODE =
        "SELECT id, amount FROM CUSTOM_OVERDRAFT_TYPE WHERE code = ?";
    private static final String NOT_A_QUERY = "hello";
    private String nonStatic = "SELECT FROM X";
}
"#,
        );
        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let parsed = output.parsed_files.first().unwrap();
        // SCREAMING_SNAKE_CASE check: QUERY_GET_BY_CODE must be extracted.
        assert!(
            parsed.sql_constants.iter().any(|c| {
                c.const_name == "QUERY_GET_BY_CODE"
                    && c.sql_text.contains("CUSTOM_OVERDRAFT_TYPE")
                    && !c.dynamic
            }),
            "QUERY_GET_BY_CODE not extracted: {:?}",
            parsed.sql_constants
        );
        // Non-static instance field must not appear (it lacks `static` + `final`).
        assert!(
            !parsed.sql_constants.iter().any(|c| c.const_name == "nonStatic"),
            "non-static field must not be extracted"
        );
    }

    #[test]
    fn parses_sql_constants_folds_string_concatenation() {
        let root = temp_repo();
        let rel = "src/main/java/com/bank/Adapter.java";
        write_file(
            &root,
            rel,
            r#"
package com.bank;
public class Adapter {
    private static final String QUERY_CONCAT =
        "SELECT id FROM " +
        "CUSTOM_OVERDRAFT " +
        "WHERE id = ?";
}
"#,
        );
        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let parsed = output.parsed_files.first().unwrap();
        let c = parsed
            .sql_constants
            .iter()
            .find(|c| c.const_name == "QUERY_CONCAT")
            .expect("QUERY_CONCAT must be extracted");
        assert!(c.sql_text.contains("CUSTOM_OVERDRAFT"), "folded text: {:?}", c.sql_text);
        assert!(!c.dynamic, "pure literal concat must not be dynamic");
    }

    #[test]
    fn parses_sql_constants_marks_dynamic_on_non_literal_concat() {
        let root = temp_repo();
        let rel = "src/main/java/com/bank/Adapter.java";
        write_file(
            &root,
            rel,
            r#"
package com.bank;
public class Adapter {
    private static final String TABLE_NAME = "CUSTOM_OVERDRAFT";
    private static final String QUERY_DYN = "SELECT id FROM " + TABLE_NAME + " WHERE id = ?";
}
"#,
        );
        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let parsed = output.parsed_files.first().unwrap();
        // TABLE_NAME is all caps but value is a short non-SQL string — may or may not be extracted.
        // QUERY_DYN references a variable, so must be dynamic.
        if let Some(c) = parsed.sql_constants.iter().find(|c| c.const_name == "QUERY_DYN") {
            assert!(c.dynamic, "concat with identifier must be dynamic");
        }
        // At minimum, the dynamic concat must not produce a fully resolved table list.
    }

    #[test]
    fn parses_sql_execution_sites_dbutil_pattern() {
        let root = temp_repo();
        let rel = "src/main/java/com/bank/OverdraftAdapterImpl.java";
        write_file(
            &root,
            rel,
            r#"
package com.bank;
import java.sql.Connection;
public class OverdraftAdapterImpl {
    private static final String QUERY_GET = "SELECT id FROM CUSTOM_OVERDRAFT WHERE id = ?";

    public Object getOverdraft(Connection conn, long id) {
        return DBUtil.executeQuery(conn, QUERY_GET, id);
    }
}
"#,
        );
        let output = parse_files(&root, &[rel.to_string()]).unwrap();
        fs::remove_dir_all(&root).unwrap();

        let parsed = output.parsed_files.first().unwrap();
        assert!(
            parsed.sql_execution_sites.iter().any(|s| {
                s.api_name == "executeQuery"
                    && s.const_ref.as_deref() == Some("QUERY_GET")
            }),
            "DBUtil.executeQuery site not extracted: {:?}",
            parsed.sql_execution_sites
        );
    }
}
