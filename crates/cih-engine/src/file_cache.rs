use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use cih_core::{NodeKind, ParsedFile, RawImport};
use cih_parse::ParsedUnit;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

const FILE_HASHES: &str = "file-hashes.json";
const PARSE_CACHE_DIR: &str = "parse-cache";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct FileHashIndex(HashMap<String, String>);

impl FileHashIndex {
    pub(crate) fn load(cih_dir: &Path) -> Self {
        let path = cih_dir.join(FILE_HASHES);
        let Ok(raw) = fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub(crate) fn from_map(map: HashMap<String, String>) -> Self {
        Self(map)
    }

    pub(crate) fn save(&self, cih_dir: &Path) -> Result<()> {
        fs::create_dir_all(cih_dir)
            .with_context(|| format!("failed to create {}", cih_dir.display()))?;
        let path = cih_dir.join(FILE_HASHES);
        let encoded = serde_json::to_string_pretty(self)?;
        fs::write(&path, encoded.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// Returns keys in `current` whose value differs from `self` (new or changed files).
    pub(crate) fn changed_files<'a>(&self, current: &'a HashMap<String, String>) -> Vec<&'a str> {
        let mut changed: Vec<&str> = current
            .iter()
            .filter_map(|(rel, hash)| match self.0.get(rel) {
                Some(previous) if previous == hash => None,
                _ => Some(rel.as_str()),
            })
            .collect();
        changed.sort_unstable();
        changed
    }

    pub(crate) fn same_file_set(&self, current: &HashMap<String, String>) -> bool {
        self.0.len() == current.len() && current.keys().all(|rel| self.0.contains_key(rel))
    }

    pub(crate) fn get(&self, rel: &str) -> Option<&str> {
        self.0.get(rel).map(String::as_str)
    }
}

/// blake3, first 16 hex chars. Reads file from disk.
pub(crate) fn hash_file(repo_root: &Path, rel: &str) -> Result<String> {
    let path = repo_root.join(rel);
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex()[..16].to_string())
}

/// Hash all readable scope files in parallel. Unreadable files are omitted so
/// the parse phase can preserve its existing "skip bad files" behavior.
pub(crate) fn hash_all(repo_root: &Path, files: &[String]) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = files
        .par_iter()
        .filter_map(|rel| match hash_file(repo_root, rel) {
            Ok(hash) => Some((rel.clone(), hash)),
            Err(err) => {
                tracing::warn!(file = rel, error = %err, "failed to hash scope file");
                None
            }
        })
        .collect();
    // Keep deterministic behavior if the input list contains duplicates.
    out.shrink_to_fit();
    out
}

pub(crate) fn load_cached_parsed(cih_dir: &Path, file_hash: &str) -> Option<ParsedUnit> {
    let path = cache_path(cih_dir, file_hash);
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub(crate) fn save_cached_parsed(
    cih_dir: &Path,
    file_hash: &str,
    parsed: &ParsedUnit,
) -> Result<()> {
    let dir = cih_dir.join(PARSE_CACHE_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let path = cache_path(cih_dir, file_hash);
    let encoded = serde_json::to_string(parsed)?;
    fs::write(&path, encoded.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn cache_path(cih_dir: &Path, file_hash: &str) -> std::path::PathBuf {
    cih_dir
        .join(PARSE_CACHE_DIR)
        .join(format!("{file_hash}.json"))
}

/// Reverse import index used to reparse changed files plus their importers.
pub(crate) struct ImporterIndex {
    importers_by_key: HashMap<String, Vec<String>>,
    keys_by_file: HashMap<String, Vec<String>>,
}

impl ImporterIndex {
    pub(crate) fn build(parsed_files: &[ParsedFile]) -> Self {
        let mut importers_by_key: HashMap<String, HashSet<String>> = HashMap::new();
        let mut keys_by_file: HashMap<String, HashSet<String>> = HashMap::new();

        for parsed in parsed_files {
            for key in exported_keys(parsed) {
                keys_by_file
                    .entry(parsed.file.clone())
                    .or_default()
                    .insert(key);
            }
            for import in &parsed.imports {
                for key in import_keys(import) {
                    importers_by_key
                        .entry(key)
                        .or_default()
                        .insert(parsed.file.clone());
                }
            }
        }

        Self {
            importers_by_key: sorted_map(importers_by_key),
            keys_by_file: sorted_map(keys_by_file),
        }
    }

    /// BFS from `changed`, expanding transitive importers up to `depth` hops.
    pub(crate) fn expand(&self, changed: &[String], depth: usize) -> HashSet<String> {
        let mut seen: HashSet<String> = changed.iter().cloned().collect();
        let mut queue: VecDeque<(String, usize)> =
            changed.iter().cloned().map(|file| (file, 0)).collect();

        while let Some((file, hop)) = queue.pop_front() {
            if hop >= depth {
                continue;
            }
            let Some(keys) = self.keys_by_file.get(&file) else {
                continue;
            };
            for key in keys {
                let Some(importers) = self.importers_by_key.get(key) else {
                    continue;
                };
                for importer in importers {
                    if seen.insert(importer.clone()) {
                        queue.push_back((importer.clone(), hop + 1));
                    }
                }
            }
        }

        seen
    }
}

fn exported_keys(parsed: &ParsedFile) -> Vec<String> {
    let mut keys = HashSet::new();
    if let Some(package) = &parsed.package {
        keys.insert(package.clone());
    }
    for def in &parsed.defs {
        if matches!(
            def.kind,
            NodeKind::Class
                | NodeKind::Interface
                | NodeKind::Enum
                | NodeKind::Record
                | NodeKind::Annotation
        ) {
            keys.insert(def.fqcn.clone());
            keys.insert(def.name.clone());
        }
    }
    let mut keys: Vec<String> = keys.into_iter().collect();
    keys.sort();
    keys
}

fn import_keys(import: &RawImport) -> Vec<String> {
    let raw = import.raw.trim();
    let mut keys = HashSet::new();
    if import.is_wildcard {
        keys.insert(raw.trim_end_matches(".*").to_string());
    } else if !raw.is_empty() {
        keys.insert(raw.to_string());
        if let Some(simple) = raw.rsplit('.').next() {
            keys.insert(simple.to_string());
        }
    }
    let mut keys: Vec<String> = keys.into_iter().collect();
    keys.sort();
    keys
}

fn sorted_map(mut input: HashMap<String, HashSet<String>>) -> HashMap<String, Vec<String>> {
    input
        .drain()
        .map(|(key, values)| {
            let mut values: Vec<String> = values.into_iter().collect();
            values.sort();
            (key, values)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cih_core::{type_id, Range, SymbolDef};
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
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn write(&self, rel: &str, content: &str) {
            let path = self.path.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn parsed(file: &str, package: &str, class_name: &str, imports: Vec<&str>) -> ParsedFile {
        let fqcn = format!("{package}.{class_name}");
        ParsedFile {
            file: file.to_string(),
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
                stereotype: None,
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
        }
    }

    fn unit(parsed_file: ParsedFile) -> ParsedUnit {
        ParsedUnit {
            rel: parsed_file.file.clone(),
            nodes: Vec::new(),
            edges: Vec::new(),
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
}
