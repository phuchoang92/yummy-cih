use cih_core::{ComplexityRecord, NodeKind, StructuralProfile};
use tree_sitter::Node as TsNode;

use super::{FileBuilder, annotation_name, annotations, first_named_child, text};

pub(super) fn class_stereotype(
    node: TsNode<'_>,
    src: &str,
    simple_name: &str,
) -> Option<&'static str> {
    for annotation in annotations(node) {
        let mapped = match annotation_name(annotation, src).as_deref() {
            Some("RestController") | Some("Controller") => "controller",
            Some("Service") => "service",
            Some("Repository") => "repository",
            Some("Configuration") => "configuration",
            Some("Component") => "component",
            Some("Entity") => "entity",
            Some("Path") => "resource",
            Some("SpringBootTest")
            | Some("ExtendWith")
            | Some("RunWith")
            | Some("WebMvcTest")
            | Some("DataJpaTest")
            | Some("DataMongoTest")
            | Some("JsonTest") => "test",
            _ => continue,
        };
        return Some(mapped);
    }
    if simple_name.ends_with("Test")
        || simple_name.ends_with("Tests")
        || simple_name.ends_with("IT")
        || simple_name.ends_with("Spec")
    {
        return Some("test");
    }
    for (suffix, stereo) in [
        ("Controller", "controller"),
        ("Endpoint", "controller"),
        ("Resource", "resource"),
        ("Api", "controller"),
        ("Handler", "handler"),
        ("Facade", "service"),
        ("Repository", "repository"),
        ("Service", "service"),
    ] {
        if simple_name.ends_with(suffix) {
            return Some(stereo);
        }
    }
    None
}

pub(super) fn is_bean_method(node: TsNode<'_>, src: &str) -> bool {
    annotations(node)
        .into_iter()
        .any(|ann| annotation_name(ann, src).as_deref() == Some("Bean"))
}

pub(super) fn is_test_method(node: TsNode<'_>, src: &str) -> bool {
    annotations(node).into_iter().any(|ann| {
        matches!(
            annotation_name(ann, src).as_deref(),
            Some("Test") | Some("ParameterizedTest") | Some("RepeatedTest")
        )
    })
}

pub(super) fn is_mock_or_injected_field(node: TsNode<'_>, src: &str) -> bool {
    annotations(node).into_iter().any(|ann| {
        matches!(
            annotation_name(ann, src).as_deref(),
            Some("MockBean")
                | Some("SpyBean")
                | Some("Autowired")
                | Some("InjectMocks")
                | Some("Mock")
        )
    })
}

pub(super) fn simple_type_name(raw: &str) -> &str {
    let s = raw.trim();
    let s = s.split('<').next().unwrap_or(s);
    let s = s.split('[').next().unwrap_or(s);
    s.trim()
}

const JPA_INTERFACES: &[&str] = &[
    "JpaRepository",
    "CrudRepository",
    "PagingAndSortingRepository",
    "ListCrudRepository",
    "ListPagingAndSortingRepository",
    "MongoRepository",
    "ReactiveCrudRepository",
    "ReactiveMongoRepository",
    "R2dbcRepository",
    "JpaSpecificationExecutor",
];

fn jpa_repository_props(node: TsNode<'_>, src: &str) -> (bool, Option<String>) {
    let Some(interfaces_node) = node.child_by_field_name("interfaces") else {
        return (false, None);
    };
    let scan_node = first_named_child(interfaces_node, "interface_type_list")
        .or_else(|| first_named_child(interfaces_node, "type_list"))
        .unwrap_or(interfaces_node);
    let mut cursor = scan_node.walk();
    for child in scan_node.named_children(&mut cursor) {
        match child.kind() {
            "type_identifier" => {
                let name = text(child, src);
                if JPA_INTERFACES.contains(&name.as_str()) {
                    return (true, None);
                }
            }
            "generic_type" => {
                let Some(name_node) = child.named_child(0) else {
                    continue;
                };
                let name = text(name_node, src);
                if JPA_INTERFACES.contains(&name.as_str()) {
                    let entity = child
                        .named_child(1)
                        .and_then(|args| args.named_child(0))
                        .map(|c| text(c, src))
                        .filter(|s| !s.is_empty());
                    return (true, entity);
                }
            }
            _ => {}
        }
    }
    (false, None)
}

pub(super) fn build_class_props(
    node: TsNode<'_>,
    src: &str,
    simple_name: &str,
) -> Option<serde_json::Value> {
    let stereotype = class_stereotype(node, src, simple_name);
    let (is_jpa, entity_opt) = jpa_repository_props(node, src);
    let effective_stereotype = stereotype.or(if is_jpa { Some("repository") } else { None });

    let table_name: Option<String> = if effective_stereotype == Some("entity") {
        let start = node.start_byte();
        let header = &src[start..src.len().min(start + 512)];
        extract_table_annotation_name(header)
    } else {
        None
    };

    let mut obj = serde_json::Map::new();
    if let Some(s) = effective_stereotype { obj.insert("stereotype".into(), s.into()); }
    if let Some(e) = entity_opt           { obj.insert("entityType".into(), e.into()); }
    if let Some(t) = table_name           { obj.insert("tableName".into(), t.into()); }
    if obj.is_empty() { None } else { Some(serde_json::Value::Object(obj)) }
}

fn extract_table_annotation_name(text: &str) -> Option<String> {
    let at_table = text.find("@Table")?;
    let after = &text[at_table + "@Table".len()..];
    let paren = after.find('(')?;
    let args = &after[paren + 1..];
    let class_pos = args.find("class ").unwrap_or(args.len());
    let search_area = &args[..class_pos.min(args.len())];
    let mut pos = 0;
    while pos < search_area.len() {
        if let Some(rel) = search_area[pos..].find("name") {
            let abs = pos + rel;
            let after_name = search_area[abs + 4..].trim_start();
            if after_name.starts_with('=') {
                let after_eq = after_name[1..].trim_start();
                if after_eq.starts_with('"') {
                    let value_start = after_eq[1..].to_string();
                    let end = value_start.find('"')?;
                    let name = value_start[..end].to_string();
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
            pos = abs + 4;
        } else {
            break;
        }
    }
    None
}

pub(super) fn attach_structural_profiles(builder: &mut FileBuilder) {
    let mut extends_count: std::collections::HashMap<&str, u16> =
        std::collections::HashMap::new();
    let mut implements_count: std::collections::HashMap<&str, u16> =
        std::collections::HashMap::new();
    for site in &builder.reference_sites {
        match site.kind {
            cih_core::RefKind::Extends => {
                *extends_count.entry(site.in_fqcn.as_str()).or_insert(0) += 1;
            }
            cih_core::RefKind::Implements => {
                *implements_count.entry(site.in_fqcn.as_str()).or_insert(0) += 1;
            }
            _ => {}
        }
    }

    let mut method_cx: std::collections::HashMap<&str, Vec<&ComplexityRecord>> =
        std::collections::HashMap::new();
    let mut method_counts: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
    let mut field_counts: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
    let mut ctor_counts: std::collections::HashMap<&str, u16> = std::collections::HashMap::new();
    for def in &builder.defs {
        match def.kind {
            NodeKind::Method => {
                *method_counts.entry(def.fqcn.as_str()).or_insert(0) += 1;
                if let Some(cx) = &def.complexity {
                    method_cx.entry(def.fqcn.as_str()).or_default().push(cx);
                }
            }
            NodeKind::Field => {
                *field_counts.entry(def.fqcn.as_str()).or_insert(0) += 1;
            }
            NodeKind::Constructor => {
                *ctor_counts.entry(def.fqcn.as_str()).or_insert(0) += 1;
                if let Some(cx) = &def.complexity {
                    method_cx.entry(def.fqcn.as_str()).or_default().push(cx);
                }
            }
            _ => {}
        }
    }

    for def in &builder.defs {
        let is_class_like = matches!(
            def.kind,
            NodeKind::Class | NodeKind::Interface | NodeKind::Enum | NodeKind::Annotation
        );
        if !is_class_like {
            continue;
        }

        let fqcn = def.fqcn.as_str();
        let cxs = method_cx.get(fqcn).map(Vec::as_slice).unwrap_or(&[]);
        let n = cxs.len() as f32;

        let avg_of = |f: fn(&ComplexityRecord) -> f32| -> f32 {
            if n == 0.0 { 0.0 } else { cxs.iter().map(|c| f(c)).sum::<f32>() / n }
        };
        let max_of = |f: fn(&ComplexityRecord) -> f32| -> f32 {
            cxs.iter().map(|c| f(c)).fold(0f32, f32::max)
        };
        let sum_of = |f: fn(&ComplexityRecord) -> f32| -> f32 {
            cxs.iter().map(|c| f(c)).sum::<f32>()
        };

        let loc = (def.range.end_line.saturating_sub(def.range.start_line)) as f32 / 1000.0;

        let features: [f32; 25] = [
            *method_counts.get(fqcn).unwrap_or(&0) as f32,
            *field_counts.get(fqcn).unwrap_or(&0) as f32,
            *ctor_counts.get(fqcn).unwrap_or(&0) as f32,
            avg_of(|c| c.cyclomatic as f32),
            max_of(|c| c.cyclomatic as f32),
            avg_of(|c| c.cognitive as f32),
            max_of(|c| c.cognitive as f32),
            avg_of(|c| c.loop_depth as f32),
            max_of(|c| c.loop_depth as f32),
            sum_of(|c| c.if_count as f32),
            sum_of(|c| c.for_count as f32),
            sum_of(|c| c.while_count as f32),
            sum_of(|c| c.switch_count as f32),
            sum_of(|c| c.try_count as f32),
            sum_of(|c| c.return_count as f32),
            sum_of(|c| c.throw_count as f32),
            def.framework_role.is_some() as u8 as f32,
            def.framework_role.is_some() as u8 as f32,
            (def.kind == NodeKind::Interface) as u8 as f32,
            def.modifiers.iter().any(|m| m == "abstract") as u8 as f32,
            (def.kind == NodeKind::Enum) as u8 as f32,
            *implements_count.get(fqcn).unwrap_or(&0) as f32,
            *extends_count.get(fqcn).unwrap_or(&0) as f32,
            (def.framework_role.as_deref() == Some("test")) as u8 as f32,
            loc.min(1.0),
        ];

        let profile = StructuralProfile { features };
        let sp_json = profile.to_json_array();

        for node in &mut builder.nodes {
            if node.id == def.id {
                let props = node.props.get_or_insert_with(|| serde_json::json!({}));
                if let serde_json::Value::Object(ref mut map) = props {
                    map.insert("sp".to_string(), sp_json);
                }
                break;
            }
        }
    }
}

