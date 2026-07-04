use std::collections::BTreeMap;

use cih_core::ModuleInfo;

use super::module_score;

fn module(frameworks: &[&str], source_files: u64) -> ModuleInfo {
    ModuleInfo {
        name: "m".to_string(),
        rel_path: ".".to_string(),
        build_file: None,
        source_files,
        source_loc: 0,
        packages: Vec::new(),
        depends_on: Vec::new(),
        frameworks: frameworks.iter().map(|s| (*s).to_string()).collect(),
        per_language: BTreeMap::new(),
    }
}

#[test]
fn module_score_treats_framework_presence_as_boolean() {
    assert_eq!(
        module_score(&module(&["spring"], 12)),
        module_score(&module(&["spring", "nestjs"], 12))
    );
    assert!(module_score(&module(&["spring"], 12)) > module_score(&module(&[], 999)));
}
