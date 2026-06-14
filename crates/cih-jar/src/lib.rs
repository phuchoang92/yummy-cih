//! JAR API-surface extraction (Phase 3, Task 8).
//!
//! Reads a `.jar` (a zip) and parses each `.class` with `cafebabe` (no decompiler,
//! no JDK) to emit **signature-only** graph nodes: Class/Interface/Enum/Annotation,
//! Method/Constructor, Field — with the SAME locked node-id scheme the app side uses
//! (`cih_core::{type_id, method_id, constructor_id, field_id}`), so an app→lib
//! `CALLS`/`USES` edge resolved in Phase 4 lands on the JAR's real method node
//! instead of dropping. No bodies, no CALLS — just the API surface.
//!
//! The 26k source-less own libs are handled **demand-driven**: pass the set of
//! referenced FQCNs (from Phase 4's unresolved references) via [`JarApiExtractor::include`]
//! so only the classes the app actually touches are emitted. `include = None` emits
//! the whole JAR (`--lib-api all`).

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use cafebabe::descriptors::{FieldDescriptor, FieldType, ReturnDescriptor};
use cafebabe::{ClassAccessFlags, MethodAccessFlags};
use cih_core::{
    constructor_id, field_id, method_id, type_id, Edge, EdgeKind, Node, NodeKind, Range,
};

/// Extracts signature-only API nodes from a JAR.
#[derive(Debug, Default, Clone)]
pub struct JarApiExtractor {
    /// Referenced FQCNs to emit (demand-driven). `None` = emit the whole JAR.
    pub include: Option<HashSet<String>>,
    /// Emit synthetic / anonymous / bridge members too. Default `false`.
    pub emit_synthetic: bool,
}

/// Result of extracting one JAR.
#[derive(Debug, Default, Clone)]
pub struct JarApiOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Number of classes emitted (after filtering).
    pub classes: u64,
    /// `.class` entries that failed to parse — skipped, never fatal.
    pub skipped: Vec<SkippedClass>,
}

#[derive(Debug, Clone)]
pub struct SkippedClass {
    pub entry: String,
    pub reason: String,
}

impl JarApiExtractor {
    /// Emit the whole JAR's public-ish API surface.
    pub fn all() -> Self {
        Self::default()
    }

    /// Demand-driven: emit only classes whose FQCN is in `include`.
    pub fn with_include(include: HashSet<String>) -> Self {
        Self {
            include: Some(include),
            emit_synthetic: false,
        }
    }

    pub fn extract(&self, jar_path: &Path) -> Result<JarApiOutput> {
        let file = std::fs::File::open(jar_path)
            .with_context(|| format!("failed to open jar {}", jar_path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("failed to read jar {}", jar_path.display()))?;

        let jar_name = jar_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let mut out = JarApiOutput::default();
        let mut buf = Vec::new();

        for i in 0..archive.len() {
            let mut entry = match archive.by_index(i) {
                Ok(entry) => entry,
                Err(err) => {
                    out.skipped.push(SkippedClass {
                        entry: format!("#{i}"),
                        reason: err.to_string(),
                    });
                    continue;
                }
            };
            if !entry.is_file() || !entry.name().ends_with(".class") {
                continue;
            }
            let name = entry.name().to_string();
            if is_info_class(&name) {
                continue;
            }

            buf.clear();
            if let Err(err) = entry.read_to_end(&mut buf) {
                out.skipped.push(SkippedClass {
                    entry: name,
                    reason: err.to_string(),
                });
                continue;
            }

            if let Err(err) = self.emit_class(&buf, &jar_name, &mut out) {
                out.skipped.push(SkippedClass {
                    entry: name,
                    reason: err,
                });
            }
        }

        normalize(&mut out);
        Ok(out)
    }

    /// Parse one `.class` and push its nodes/edges. Returns `Err(reason)` so the
    /// caller records a skip rather than aborting the whole JAR.
    fn emit_class(&self, bytes: &[u8], jar_name: &str, out: &mut JarApiOutput) -> Result<(), String> {
        // Signatures only — no need to parse method bodies.
        let mut opts = cafebabe::ParseOptions::default();
        opts.parse_bytecode(false);
        let class = cafebabe::parse_class_with_options(bytes, &opts)
            .map_err(|e| format!("cafebabe parse failed: {e}"))?;

        let Some(kind) = class_kind(class.access_flags) else {
            return Ok(()); // module-info or otherwise not a type we model
        };

        let internal = &*class.this_class;
        if !self.emit_synthetic
            && (class.access_flags.contains(ClassAccessFlags::SYNTHETIC)
                || is_anonymous_or_local(internal))
        {
            return Ok(());
        }

        let fqcn = internal_to_fqcn(internal);
        if let Some(include) = &self.include {
            if !include.contains(&fqcn) {
                return Ok(());
            }
        }

        let simple_name = fqcn.rsplit('.').next().unwrap_or(&fqcn).to_string();
        let class_id = type_id(kind, &fqcn);
        out.nodes.push(Node {
            id: class_id.clone(),
            kind,
            name: simple_name,
            qualified_name: Some(fqcn.clone()),
            file: jar_name.to_string(),
            range: Range::default(),
            props: Some(serde_json::json!({
                "fromJar": true,
                "external": true,
                "jar": jar_name,
            })),
        });
        out.classes += 1;

        for method in &class.methods {
            if method.name == "<clinit>" {
                continue; // static initializer — not API
            }
            if !self.emit_synthetic
                && method.access_flags.intersects(
                    MethodAccessFlags::SYNTHETIC | MethodAccessFlags::BRIDGE,
                )
            {
                continue;
            }

            let arity = method.descriptor.parameters.len() as u16;
            let params: Vec<String> = method
                .descriptor
                .parameters
                .iter()
                .map(render_field)
                .collect();
            let returns = render_return(&method.descriptor.return_type);

            let (member_id, member_kind, member_name) = if method.name == "<init>" {
                (constructor_id(&fqcn, arity), NodeKind::Constructor, "<init>".to_string())
            } else {
                (
                    method_id(&fqcn, &method.name, arity),
                    NodeKind::Method,
                    method.name.to_string(),
                )
            };

            let qualified_name = format!("{fqcn}#{member_name}/{arity}");
            out.nodes.push(Node {
                id: member_id.clone(),
                kind: member_kind,
                name: member_name,
                qualified_name: Some(qualified_name),
                file: jar_name.to_string(),
                range: Range::default(),
                props: Some(serde_json::json!({
                    "fromJar": true,
                    "external": true,
                    "params": params,
                    "returns": returns,
                })),
            });
            out.edges.push(Edge {
                src: class_id.clone(),
                dst: member_id,
                kind: EdgeKind::HasMethod,
                confidence: 1.0,
                reason: "jar-member".into(),
            });
        }

        for jar_field in &class.fields {
            if !self.emit_synthetic
                && jar_field
                    .access_flags
                    .contains(cafebabe::FieldAccessFlags::SYNTHETIC)
            {
                continue;
            }
            let field_name = jar_field.name.to_string();
            let id = field_id(&fqcn, &field_name);
            out.nodes.push(Node {
                id: id.clone(),
                kind: NodeKind::Field,
                name: field_name.clone(),
                qualified_name: Some(format!("{fqcn}#{field_name}")),
                file: jar_name.to_string(),
                range: Range::default(),
                props: Some(serde_json::json!({
                    "fromJar": true,
                    "external": true,
                    "type": render_field(&jar_field.descriptor),
                })),
            });
            out.edges.push(Edge {
                src: class_id.clone(),
                dst: id,
                kind: EdgeKind::HasField,
                confidence: 1.0,
                reason: "jar-member".into(),
            });
        }

        Ok(())
    }
}

fn class_kind(flags: ClassAccessFlags) -> Option<NodeKind> {
    if flags.contains(ClassAccessFlags::MODULE) {
        return None;
    }
    if flags.contains(ClassAccessFlags::ANNOTATION) {
        Some(NodeKind::Annotation)
    } else if flags.contains(ClassAccessFlags::INTERFACE) {
        Some(NodeKind::Interface)
    } else if flags.contains(ClassAccessFlags::ENUM) {
        Some(NodeKind::Enum)
    } else {
        // Records carry a `Record` attribute, not an access flag; cafebabe does not
        // surface it simply, so a record lands here as `Class`. Acceptable for the
        // API surface (its accessor methods/fields are still emitted).
        Some(NodeKind::Class)
    }
}

/// JVM internal name (`com/acme/Outer$Inner`) -> FQCN (`com.acme.Outer.Inner`).
fn internal_to_fqcn(internal: &str) -> String {
    internal.replace(['/', '$'], ".")
}

/// True for anonymous (`Outer$1`) and local classes — any `$`-segment that is
/// all ASCII digits, or starts with a digit (local class `Outer$1Local`).
fn is_anonymous_or_local(internal: &str) -> bool {
    let simple = internal.rsplit('/').next().unwrap_or(internal);
    simple.split('$').skip(1).any(|seg| {
        seg.chars().next().is_some_and(|c| c.is_ascii_digit())
    })
}

fn is_info_class(entry_name: &str) -> bool {
    let file = entry_name.rsplit('/').next().unwrap_or(entry_name);
    file == "module-info.class" || file == "package-info.class"
}

/// Render a field/parameter descriptor as a human type, e.g. `java.lang.String[]`.
fn render_field(descriptor: &FieldDescriptor) -> String {
    let base = match &descriptor.field_type {
        FieldType::Byte => "byte".to_string(),
        FieldType::Char => "char".to_string(),
        FieldType::Double => "double".to_string(),
        FieldType::Float => "float".to_string(),
        FieldType::Integer => "int".to_string(),
        FieldType::Long => "long".to_string(),
        FieldType::Short => "short".to_string(),
        FieldType::Boolean => "boolean".to_string(),
        FieldType::Object(name) => internal_to_fqcn(name),
    };
    format!("{base}{}", "[]".repeat(descriptor.dimensions as usize))
}

fn render_return(ret: &ReturnDescriptor) -> String {
    match ret {
        ReturnDescriptor::Void => "void".to_string(),
        ReturnDescriptor::Return(field) => render_field(field),
    }
}

fn normalize(out: &mut JarApiOutput) {
    out.nodes.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    out.nodes.dedup_by(|a, b| a.id == b.id);
    out.edges.sort_by(|a, b| {
        a.src
            .as_str()
            .cmp(b.src.as_str())
            .then(a.dst.as_str().cmp(b.dst.as_str()))
            .then(a.kind.cypher_label().cmp(b.kind.cypher_label()))
    });
    out.edges
        .dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);
    out.skipped.sort_by(|a, b| a.entry.cmp(&b.entry));
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::NodeId;
    use std::path::PathBuf;

    fn sample_jar() -> PathBuf {
        PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample.jar"
        ))
    }

    fn has_node(out: &JarApiOutput, id: &NodeId) -> bool {
        out.nodes.iter().any(|n| &n.id == id)
    }

    fn has_edge(out: &JarApiOutput, kind: EdgeKind, src: &NodeId, dst: &NodeId) -> bool {
        out.edges
            .iter()
            .any(|e| e.kind == kind && &e.src == src && &e.dst == dst)
    }

    #[test]
    fn extracts_api_with_ids_matching_the_locked_scheme() {
        let out = JarApiExtractor::all().extract(&sample_jar()).unwrap();
        assert!(out.skipped.is_empty(), "skipped: {:?}", out.skipped);

        let sample = type_id(NodeKind::Class, "com.acme.Sample");
        let inner = type_id(NodeKind::Class, "com.acme.Sample.Inner");

        // These ids are exactly what the app side would resolve a call/ctor/field to.
        assert!(has_node(&out, &sample));
        assert!(has_node(&out, &field_id("com.acme.Sample", "count")));
        assert!(has_node(&out, &constructor_id("com.acme.Sample", 1)));
        assert!(has_node(&out, &method_id("com.acme.Sample", "greet", 1)));
        assert!(has_node(&out, &method_id("com.acme.Sample", "make", 0)));
        assert!(has_node(&out, &inner));
        assert!(has_node(&out, &method_id("com.acme.Sample.Inner", "ping", 0)));

        // HAS_METHOD / HAS_FIELD wire members to their owning class.
        assert!(has_edge(
            &out,
            EdgeKind::HasMethod,
            &sample,
            &method_id("com.acme.Sample", "greet", 1)
        ));
        assert!(has_edge(
            &out,
            EdgeKind::HasField,
            &sample,
            &field_id("com.acme.Sample", "count")
        ));

        // Anonymous Sample$1 is skipped by default (no `com.acme.Sample.1` type).
        assert!(!has_node(&out, &type_id(NodeKind::Class, "com.acme.Sample.1")));

        // Nodes are tagged as external/from-jar; descriptor types are rendered.
        let greet = out
            .nodes
            .iter()
            .find(|n| n.id == method_id("com.acme.Sample", "greet", 1))
            .unwrap();
        let props = greet.props.as_ref().unwrap();
        assert_eq!(props.get("fromJar").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(props.get("external").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            props.get("returns").and_then(|v| v.as_str()),
            Some("java.lang.String")
        );
        assert_eq!(
            props
                .get("params")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>()),
            Some(vec!["int"])
        );
    }

    #[test]
    fn demand_driven_include_emits_only_requested_classes() {
        let include = HashSet::from(["com.acme.Sample.Inner".to_string()]);
        let out = JarApiExtractor::with_include(include)
            .extract(&sample_jar())
            .unwrap();

        assert!(has_node(&out, &type_id(NodeKind::Class, "com.acme.Sample.Inner")));
        assert!(has_node(&out, &method_id("com.acme.Sample.Inner", "ping", 0)));
        // The unreferenced top-level class is NOT emitted.
        assert!(!has_node(&out, &type_id(NodeKind::Class, "com.acme.Sample")));
        assert_eq!(out.classes, 1);
    }
}
