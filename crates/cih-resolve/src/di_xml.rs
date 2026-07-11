//! Phase 2a — Spring / Blueprint DI resolution from XML wiring.
//!
//! Parses Spring `applicationContext*.xml` / `beans.xml` and OSGi Blueprint XML
//! `<bean>` / `<reference>` / `<service>` definitions, then matches injected Java
//! field types against the declared bean classes. When a field type `T` resolves
//! to a bean class `C`, we emit a `CALLS` edge from the containing class to `C`
//! (the wiring the DI container performs at runtime, invisible to the
//! pure-source resolver).
//!
//! This runs during `analyze` AFTER the main Java parse/resolve phase, so it can
//! access the `ParsedFile` list for type bindings.
//!
//! Like `integration_xml.rs`, this is a deliberately lightweight text scanner: we
//! do not pull in an XML parser dependency. Malformed input simply yields fewer
//! facts; it never panics.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use cih_core::{type_id, BindingKind, Edge, EdgeKind, Node, NodeId, NodeKind, ParsedFile, Range};

pub struct DiXmlOutput {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// A bean definition parsed from a DI XML file.
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct BeanDef {
    #[allow(dead_code)]
    pub id: Option<String>,
    pub fqcn: String,
    pub file: String,
}

/// A `<reference interface="...">` lookup parsed from Blueprint or Spring-DM
/// (`<osgi:reference>`) XML — the namespace prefix is stripped during parsing.
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct ReferenceDef {
    pub interface: String,
}

/// Returns true when the file content looks like a Spring / Blueprint / Spring-DM DI config.
fn is_di_xml(content: &str) -> bool {
    content.contains("http://www.springframework.org/schema/beans")
        || content.contains("http://www.osgi.org/xmlns/blueprint")
        || content.contains("http://www.springframework.org/schema/osgi")
}

/// Returns true when a repo-relative path matches one of the DI XML name patterns:
/// `applicationContext*.xml`, `beans.xml`, `blueprint*.xml`, `OSGI-INF/blueprint/*.xml`,
/// `META-INF/spring/*.xml` (OSGi bundles, e.g. SAP-OCB `bundle-context-*.xml` /
/// `beans_rest*.xml` — the [`is_di_xml`] content gate is the real filter there).
#[doc(hidden)]
pub fn is_di_xml_path(rel: &str) -> bool {
    let file_name = rel.rsplit('/').next().unwrap_or(rel);
    if file_name.eq_ignore_ascii_case("beans.xml") {
        return true;
    }
    let lower = file_name.to_ascii_lowercase();
    if lower.starts_with("applicationcontext") && lower.ends_with(".xml") {
        return true;
    }
    if lower.starts_with("blueprint") && lower.ends_with(".xml") {
        return true;
    }
    if rel.contains("OSGI-INF/blueprint/") && lower.ends_with(".xml") {
        return true;
    }
    if rel.contains("META-INF/spring/") && lower.ends_with(".xml") {
        return true;
    }
    false
}

/// Extract `<bean … class="…">` and `<reference interface="…">` definitions from
/// one DI XML document. (`<service>` declarations expose an existing bean and do
/// not introduce a new wiring target, so they are not collected here.)
#[doc(hidden)]
pub fn parse_di_document(rel: &str, content: &str) -> (Vec<BeanDef>, Vec<ReferenceDef>) {
    let mut beans = Vec::new();
    let mut references = Vec::new();

    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let tag_start = i;
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let name_start = i;
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && bytes[i] != b'>'
                && bytes[i] != b'/'
            {
                i += 1;
            }
            let tag_name = &content[name_start..i];
            // Strip any namespace prefix (`beans:bean` → `bean`).
            let local = tag_name.rsplit(':').next().unwrap_or(tag_name);

            match local {
                "bean" => {
                    if let Some(class) = extract_xml_attr(&content[tag_start..], "class") {
                        let id = extract_xml_attr(&content[tag_start..], "id")
                            .or_else(|| extract_xml_attr(&content[tag_start..], "name"));
                        beans.push(BeanDef {
                            id,
                            fqcn: class,
                            file: rel.to_string(),
                        });
                    }
                }
                "reference" => {
                    if let Some(interface) = extract_xml_attr(&content[tag_start..], "interface") {
                        references.push(ReferenceDef { interface });
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }

    (beans, references)
}

/// Simple (unqualified) name of a possibly-qualified type/raw type, with generics
/// and array markers stripped: `List<Foo>` → `List`, `Foo[]` → `Foo`,
/// `com.acme.Foo` → `Foo`.
#[doc(hidden)]
pub fn simple_name(raw: &str) -> &str {
    let raw = raw.trim();
    let before_generic = raw.split('<').next().unwrap_or(raw);
    let before_array = before_generic.split('[').next().unwrap_or(before_generic);
    let simple = before_array.rsplit('.').next().unwrap_or(before_array);
    simple.trim()
}

/// Build a CALLS edge to a bean class node.
fn calls_edge(src: NodeId, dst_kind: NodeKind, dst_fqcn: &str, reason: &str) -> Edge {
    Edge {
        src,
        dst: type_id(dst_kind, dst_fqcn),
        kind: EdgeKind::Calls,
        confidence: 0.7,
        reason: reason.to_string(),
        props: None,
    }
}

/// Extract DI wiring facts across the repo.
///
/// 1. Walk the repo root for DI XML files (`applicationContext*.xml`, `beans.xml`,
///    `blueprint*.xml`, `OSGI-INF/blueprint/*.xml`).
/// 2. Build a `simple-name → bean FQCN` map across all discovered DI XML files.
/// 3. For each `Field` [`TypeBinding`] in the parsed files, match its `raw_type`
///    simple name against a known bean class and emit a `CALLS` edge from the
///    containing class to the bean class.
/// 4. For each Blueprint `<reference interface="I">`, look up who implements `I`
///    among the parsed classes and emit a `CALLS` edge from `I` to each implementor.
pub fn extract_di_xml(repo_root: &Path, parsed: &[ParsedFile]) -> DiXmlOutput {
    let (beans, references) = collect_di_definitions(repo_root);

    if beans.is_empty() && references.is_empty() {
        return DiXmlOutput {
            nodes: vec![],
            edges: vec![],
        };
    }

    // Index bean classes by their simple name (multiple beans may share a class).
    let mut beans_by_simple: HashMap<&str, Vec<&BeanDef>> = HashMap::new();
    for bean in &beans {
        beans_by_simple
            .entry(simple_name(&bean.fqcn))
            .or_default()
            .push(bean);
    }

    // Index parsed types by FQCN simple name → list of (fqcn, node kind), and map
    // each implemented/extended interface simple name → its implementors.
    let mut types_by_simple: HashMap<&str, Vec<(&str, NodeKind)>> = HashMap::new();
    let mut owner_kind_by_fqcn: HashMap<&str, NodeKind> = HashMap::new();
    for pf in parsed {
        for def in &pf.defs {
            if matches!(
                def.kind,
                NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Record
            ) {
                types_by_simple
                    .entry(simple_name(&def.fqcn))
                    .or_default()
                    .push((def.fqcn.as_str(), def.kind));
                owner_kind_by_fqcn.insert(def.fqcn.as_str(), def.kind);
            }
        }
    }

    // interface-simple-name → implementor (fqcn, kind).
    let mut implementors: HashMap<&str, Vec<(&str, NodeKind)>> = HashMap::new();
    for pf in parsed {
        for site in &pf.reference_sites {
            if !matches!(
                site.kind,
                cih_core::RefKind::Implements | cih_core::RefKind::Extends
            ) {
                continue;
            }
            // The enclosing type is the implementor. `in_fqcn` for heritage sites
            // is `fqcn#…`; take the class FQCN before the `#`.
            let owner_fqcn = site.in_fqcn.split('#').next().unwrap_or(&site.in_fqcn);
            let kind = owner_kind_by_fqcn
                .get(owner_fqcn)
                .copied()
                .unwrap_or(NodeKind::Class);
            implementors
                .entry(simple_name(&site.name))
                .or_default()
                .push((owner_fqcn, kind));
        }
    }

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen_bean_nodes: HashSet<String> = HashSet::new();

    // Emit a Class node for every bean class so the CALLS edge has a destination,
    // even if the bean class itself was not parsed (it may live in a JAR).
    let mut emit_bean_node = |fqcn: &str, file: &str, nodes: &mut Vec<Node>| {
        let id = type_id(NodeKind::Class, fqcn);
        if seen_bean_nodes.insert(id.as_str().to_string()) {
            nodes.push(Node {
                id,
                kind: NodeKind::Class,
                name: simple_name(fqcn).to_string(),
                qualified_name: Some(fqcn.to_string()),
                file: file.to_string(),
                range: Range::default(),
                props: Some(serde_json::json!({
                    "source": "di_xml",
                    "di_bean": true,
                })),
            });
        }
    };

    // 3. Field-injection bindings → CALLS edge from containing class to bean class.
    //    `Field` bindings carry the enclosing type FQCN in `in_fqcn`.
    for pf in parsed {
        for binding in &pf.type_bindings {
            if binding.kind != BindingKind::Field {
                continue;
            }
            let field_simple = simple_name(&binding.raw_type);
            let Some(candidates) = beans_by_simple.get(field_simple) else {
                continue;
            };
            let owner_fqcn = binding
                .in_fqcn
                .split('#')
                .next()
                .unwrap_or(&binding.in_fqcn);
            let owner_kind = owner_kind_by_fqcn
                .get(owner_fqcn)
                .copied()
                .unwrap_or(NodeKind::Class);
            let src = type_id(owner_kind, owner_fqcn);

            for bean in candidates {
                if bean.fqcn == owner_fqcn {
                    continue; // no self-edge
                }
                emit_bean_node(&bean.fqcn, &bean.file, &mut nodes);
                edges.push(calls_edge(
                    src.clone(),
                    NodeKind::Class,
                    &bean.fqcn,
                    "di-xml-bean-field",
                ));
            }
        }
    }

    // 4. Blueprint `<reference interface="I">` → CALLS edge from I to each implementor.
    for reference in &references {
        let iface_simple = simple_name(&reference.interface);
        let Some(impls) = implementors.get(iface_simple) else {
            continue;
        };
        let Some(iface_types) = types_by_simple.get(iface_simple) else {
            continue;
        };
        for (impl_fqcn, impl_kind) in impls {
            emit_bean_node(impl_fqcn, "", &mut nodes);
            for (iface_fqcn, iface_kind) in iface_types {
                if iface_fqcn == impl_fqcn {
                    continue;
                }
                edges.push(calls_edge(
                    type_id(*iface_kind, iface_fqcn),
                    *impl_kind,
                    impl_fqcn,
                    "di-xml-blueprint-reference",
                ));
            }
        }
    }

    // Deduplicate edges by (src, dst, kind).
    edges.sort_by(|a, b| {
        a.src
            .as_str()
            .cmp(b.src.as_str())
            .then_with(|| a.dst.as_str().cmp(b.dst.as_str()))
            .then_with(|| a.kind.cypher_label().cmp(b.kind.cypher_label()))
    });
    edges.dedup_by(|a, b| {
        a.src == b.src && a.dst == b.dst && a.kind.cypher_label() == b.kind.cypher_label()
    });

    DiXmlOutput { nodes, edges }
}

/// Walk `repo_root` for DI XML files and parse their bean/reference definitions.
/// Best-effort: unreadable files and walk errors are skipped with a warning.
fn collect_di_definitions(repo_root: &Path) -> (Vec<BeanDef>, Vec<ReferenceDef>) {
    use rayon::prelude::*;

    // Collect candidate DI XML paths sequentially — the ignore walker is not Sync.
    let candidates: Vec<(std::path::PathBuf, String)> = {
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
                    if !is_xml {
                        return None;
                    }
                    let rel = path
                        .strip_prefix(repo_root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if is_di_xml_path(&rel) {
                        Some((path, rel))
                    } else {
                        None
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "di-xml: walk error — skipping");
                    None
                }
                _ => None,
            })
            .collect()
    };

    // Read and parse candidate files in parallel.
    let results: Vec<(Vec<BeanDef>, Vec<ReferenceDef>)> = candidates
        .par_iter()
        .filter_map(|(path, rel)| {
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!(file = %rel, error = %err, "di-xml: read failed — skipping");
                    return None;
                }
            };
            if !is_di_xml(&content) {
                return None;
            }
            Some(parse_di_document(rel, &content))
        })
        .collect();

    let mut beans: Vec<BeanDef> = Vec::new();
    let mut references: Vec<ReferenceDef> = Vec::new();
    for (file_beans, file_refs) in results {
        beans.extend(file_beans);
        references.extend(file_refs);
    }

    (beans, references)
}

/// Extract a named XML attribute value from a tag fragment. Handles single and
/// double quoted values. Matches only at an attribute boundary so `class=` does
/// not match a longer attribute like `myclass=`.
#[doc(hidden)]
pub fn extract_xml_attr(tag_fragment: &str, attr_name: &str) -> Option<String> {
    let search_in = &tag_fragment[..tag_fragment.len().min(2000)];
    let needle = format!("{attr_name}=");
    let mut from = 0;
    loop {
        let rel = search_in[from..].find(&needle)?;
        let pos = from + rel;
        let prev_ok = pos == 0
            || search_in.as_bytes()[pos - 1].is_ascii_whitespace()
            || search_in.as_bytes()[pos - 1] == b'<';
        if prev_ok {
            let after = &search_in[pos + needle.len()..];
            let first = after.chars().next()?;
            if first == '"' || first == '\'' {
                let end = after[1..].find(first)?;
                return Some(after[1..end + 1].to_string());
            }
            return None;
        }
        from = pos + needle.len();
    }
}
