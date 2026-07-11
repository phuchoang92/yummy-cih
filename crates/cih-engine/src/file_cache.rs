use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use cih_core::NodeKind;
#[doc(hidden)]
pub use cih_core::{ParsedFile, RawImport};
#[doc(hidden)]
pub use cih_parse::ParsedUnit;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

const FILE_HASHES: &str = "file-hashes.json";
const PARSE_CACHE_DIR: &str = "parse-cache";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FileHashIndex(HashMap<String, String>);

impl FileHashIndex {
    pub fn load(cih_dir: &Path) -> Self {
        let path = cih_dir.join(FILE_HASHES);
        let Ok(raw) = fs::read_to_string(&path) else {
            return Self::default();
        };
        serde_json::from_str(&raw).unwrap_or_default()
    }

    pub fn from_map(map: HashMap<String, String>) -> Self {
        Self(map)
    }

    pub fn save(&self, cih_dir: &Path) -> Result<()> {
        fs::create_dir_all(cih_dir)
            .with_context(|| format!("failed to create {}", cih_dir.display()))?;
        let path = cih_dir.join(FILE_HASHES);
        let encoded = serde_json::to_string_pretty(self)?;
        fs::write(&path, encoded.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// Returns keys in `current` whose value differs from `self` (new or changed files).
    pub fn changed_files<'a>(&self, current: &'a HashMap<String, String>) -> Vec<&'a str> {
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

    pub fn same_file_set(&self, current: &HashMap<String, String>) -> bool {
        self.0.len() == current.len() && current.keys().all(|rel| self.0.contains_key(rel))
    }

    pub fn get(&self, rel: &str) -> Option<&str> {
        self.0.get(rel).map(String::as_str)
    }
}

/// blake3, first 16 hex chars. Reads file from disk.
pub fn hash_file(repo_root: &Path, rel: &str) -> Result<String> {
    let path = repo_root.join(rel);
    let bytes = fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex()[..16].to_string())
}

/// Hash all readable scope files in parallel. Unreadable files are omitted so
/// the parse phase can preserve its existing "skip bad files" behavior.
pub fn hash_all(repo_root: &Path, files: &[String]) -> HashMap<String, String> {
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

pub fn load_cached_parsed(cih_dir: &Path, schema: u32, file_hash: &str) -> Option<ParsedUnit> {
    let path = cache_path(cih_dir, schema, file_hash);
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn save_cached_parsed(
    cih_dir: &Path,
    schema: u32,
    file_hash: &str,
    parsed: &ParsedUnit,
) -> Result<()> {
    let path = cache_path(cih_dir, schema, file_hash);
    let dir = path.parent().expect("cache path always has a parent");
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let encoded = serde_json::to_string(parsed)?;
    fs::write(&path, encoded.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Prepare the versioned parse-cache dir: create `parse-cache/v<schema>/` and
/// prune every OTHER schema's dir plus legacy flat (pre-versioning) entries.
/// Cached units are only valid for the parser schema that wrote them — a
/// schema bump means every parser output may differ, so stale entries must
/// never be readable again.
pub fn prepare_parse_cache(cih_dir: &Path, schema: u32) -> Result<()> {
    let root = cih_dir.join(PARSE_CACHE_DIR);
    let keep = format!("v{schema}");
    let mut pruned = false;
    if root.exists() {
        // Other schema dirs.
        let had_others = fs::read_dir(&root)
            .map(|entries| {
                entries.flatten().any(|entry| {
                    entry.path().is_dir() && entry.file_name().to_str() != Some(keep.as_str())
                })
            })
            .unwrap_or(false);
        if had_others {
            crate::versioning::prune_other_versions(&root, &keep)?;
            pruned = true;
        }
        // Legacy flat `<hash>.json` files from before versioning (implicit v1).
        for entry in fs::read_dir(&root)
            .with_context(|| format!("failed to read {}", root.display()))?
            .flatten()
        {
            let path = entry.path();
            if path.is_file() {
                if let Err(err) = fs::remove_file(&path) {
                    tracing::warn!(path = %path.display(), error = %err, "failed to prune legacy parse-cache file");
                } else {
                    pruned = true;
                }
            }
        }
    }
    fs::create_dir_all(root.join(&keep))
        .with_context(|| format!("failed to create {}", root.join(&keep).display()))?;
    if pruned {
        tracing::info!(
            schema,
            "parse cache schema v{schema} — stale cache cleared, this run re-parses all files"
        );
    }
    Ok(())
}

fn cache_path(cih_dir: &Path, schema: u32, file_hash: &str) -> std::path::PathBuf {
    cih_dir
        .join(PARSE_CACHE_DIR)
        .join(format!("v{schema}"))
        .join(format!("{file_hash}.json"))
}

/// Reverse import index used to reparse changed files plus their importers.
pub struct ImporterIndex {
    importers_by_key: HashMap<String, Vec<String>>,
    keys_by_file: HashMap<String, Vec<String>>,
}

impl ImporterIndex {
    pub fn build(parsed_files: &[ParsedFile]) -> Self {
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
    pub fn expand(&self, changed: &[String], depth: usize) -> HashSet<String> {
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
