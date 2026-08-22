use cih_core::{EdgeKind, NodeKind};
use cih_lang::{c::CProvider, lang_for_path, language_ids_for_paths, LanguageProvider};

#[test]
fn c_provider_is_selected_for_dot_c_files() {
    assert_eq!(lang_for_path("drivers/net/probe.c"), "c");
    let languages = language_ids_for_paths(&["drivers/net/probe.c"]);
    assert!(languages.contains("c"));
    assert!(!languages.contains("cpp"));
}

#[test]
fn parses_c_struct_function_and_include() {
    let source = r#"
#include "device.h"

struct device {
    int id;
};

static int probe(struct device *device) {
    return device->id;
}
"#;
    let unit = CProvider::new()
        .parse_file("drivers/net/probe.c", source)
        .unwrap();

    assert_eq!(unit.parsed_file.language, "c");
    assert!(unit
        .nodes
        .iter()
        .any(|node| node.kind == NodeKind::Class && node.name == "device"));
    assert!(unit
        .nodes
        .iter()
        .any(|node| node.kind == NodeKind::Function && node.name == "probe"));
    assert!(unit
        .edges
        .iter()
        .any(|edge| edge.kind == EdgeKind::Contains));
    assert!(unit
        .parsed_file
        .imports
        .iter()
        .any(|import| import.raw == "device.h"));
}
