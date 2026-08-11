use cih_core::{Node, NodeId, NodeKind, ParsedFile, ParsedUnit, Range};
use cih_parse::parse_output_from_units;

fn callable(id: &str, kind: NodeKind, file: &str) -> Node {
    Node {
        id: NodeId::new(id),
        kind,
        name: id.to_string(),
        qualified_name: None,
        file: file.to_string(),
        range: Range::default(),
        props: None,
    }
}

fn unit(language: &str, denominator: u32, nodes: Vec<Node>) -> ParsedUnit {
    ParsedUnit {
        rel: format!("fixture.{language}"),
        nodes,
        edges: Vec::new(),
        parsed_file: ParsedFile {
            file: format!("fixture.{language}"),
            language: language.to_string(),
            ..Default::default()
        },
        syntactic_callables: denominator,
    }
}

#[test]
fn measured_numerator_only_counts_nodes_from_measured_units() {
    let output = parse_output_from_units(
        vec![
            unit(
                "typescript",
                2,
                vec![callable(
                    "Function:fixture#measured/0",
                    NodeKind::Function,
                    "fixture.ts",
                )],
            ),
            unit(
                "rust",
                0,
                vec![
                    callable(
                        "Function:fixture#unmeasured/0",
                        NodeKind::Function,
                        "fixture.rs",
                    ),
                    callable(
                        "Method:fixture#also_unmeasured/0",
                        NodeKind::Method,
                        "fixture.rs",
                    ),
                ],
            ),
        ],
        Vec::new(),
    );

    assert_eq!(output.syntactic_callables, 2);
    assert_eq!(output.measured_callable_node_count, 1);
    assert_eq!(output.callable_measurements_by_language.len(), 1);
    assert_eq!(
        output.callable_measurements_by_language["typescript"].measured_callable_node_count,
        1
    );
}
