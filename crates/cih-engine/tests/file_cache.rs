use cih_core::{type_id, NodeKind, Range, SymbolDef};
use cih_engine_lib::file_cache::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "cih-file-cache-test-{}-{unique}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.path.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn parsed(file: &str, package: &str, class_name: &str, imports: Vec<&str>) -> ParsedFile {
    let fqcn = format!("{package}.{class_name}");
    ParsedFile {
        file: file.to_string(),
        language: String::new(),
        package: Some(package.to_string()),
        defs: vec![SymbolDef {
            id: type_id(NodeKind::Class, &fqcn),
            kind: NodeKind::Class,
            fqcn,
            name: class_name.to_string(),
            owner: None,
            range: Range::default(),
            modifiers: Vec::new(),
            param_types: Vec::new(),
            return_type: None,
            declared_type: None,
            framework_role: None,
            body_fingerprint: None,
            complexity: None,
            lang_meta: None,
        }],
        imports: imports
            .into_iter()
            .map(|raw| RawImport {
                raw: raw.to_string(),
                is_static: false,
                is_wildcard: raw.ends_with(".*"),
                range: Range::default(),
            })
            .collect(),
        reference_sites: Vec::new(),
        type_bindings: Vec::new(),
        contract_sites: Vec::new(),
        sql_constants: Vec::new(),
        sql_execution_sites: Vec::new(),
        string_constants: vec![],
    }
}

fn unit(parsed_file: ParsedFile) -> ParsedUnit {
    ParsedUnit {
        rel: parsed_file.file.clone(),
        nodes: Vec::new(),
        edges: Vec::new(),
        import_bindings: Vec::new(),
        parsed_file,
    }
}

#[test]
fn file_hash_index_round_trips() {
    let tmp = TempDir::new();
    let mut map = HashMap::new();
    map.insert("A.java".to_string(), "abc".to_string());
    map.insert("B.java".to_string(), "def".to_string());

    FileHashIndex::from_map(map).save(&tmp.path).unwrap();
    let loaded = FileHashIndex::load(&tmp.path);

    assert_eq!(loaded.get("A.java"), Some("abc"));
    assert_eq!(loaded.get("B.java"), Some("def"));
}

#[test]
fn changed_files_detects_addition_and_modification() {
    let previous = FileHashIndex::from_map(HashMap::from([
        ("A.java".to_string(), "1".to_string()),
        ("B.java".to_string(), "2".to_string()),
    ]));
    let current = HashMap::from([
        ("A.java".to_string(), "1".to_string()),
        ("B.java".to_string(), "changed".to_string()),
        ("C.java".to_string(), "3".to_string()),
    ]);

    assert_eq!(previous.changed_files(&current), vec!["B.java", "C.java"]);
}

#[test]
fn parse_cache_round_trips() {
    let tmp = TempDir::new();
    let parsed_file = parsed("A.java", "com.acme", "A", vec![]);
    let unit = unit(parsed_file.clone());

    save_cached_parsed(&tmp.path, "abc123", &unit).unwrap();
    let loaded = load_cached_parsed(&tmp.path, "abc123").unwrap();

    assert_eq!(loaded.rel, "A.java");
    assert_eq!(loaded.parsed_file, parsed_file);
}

#[test]
fn importer_index_bfs_depth_1() {
    let a = parsed("A.java", "com.acme", "A", vec!["com.acme.B"]);
    let b = parsed("B.java", "com.acme", "B", vec![]);
    let index = ImporterIndex::build(&[a, b]);

    let expanded = index.expand(&["B.java".to_string()], 1);

    assert!(expanded.contains("A.java"));
    assert!(expanded.contains("B.java"));
}

#[test]
fn importer_index_bfs_respects_depth() {
    let a = parsed("A.java", "com.acme", "A", vec![]);
    let b = parsed("B.java", "com.acme", "B", vec!["com.acme.A"]);
    let c = parsed("C.java", "com.acme", "C", vec!["com.acme.B"]);
    let d = parsed("D.java", "com.acme", "D", vec!["com.acme.C"]);
    let index = ImporterIndex::build(&[a, b, c, d]);

    let expanded = index.expand(&["A.java".to_string()], 2);

    assert!(expanded.contains("A.java"));
    assert!(expanded.contains("B.java"));
    assert!(expanded.contains("C.java"));
    assert!(!expanded.contains("D.java"));
}

#[test]
fn hash_all_hashes_readable_files() {
    let tmp = TempDir::new();
    tmp.write("A.java", "class A {}\n");
    tmp.write("B.java", "class B {}\n");

    let hashes = hash_all(&tmp.path, &["A.java".into(), "B.java".into()]);

    assert_eq!(hashes.len(), 2);
    assert_eq!(hashes.get("A.java").unwrap().len(), 16);
    assert_ne!(hashes.get("A.java"), hashes.get("B.java"));
}
