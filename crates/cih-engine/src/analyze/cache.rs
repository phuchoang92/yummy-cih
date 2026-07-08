use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use cih_core::{GraphArtifacts, ParsedUnit};
use cih_parse::ParseOutput;

use crate::file_cache::{
    hash_all, load_cached_parsed, save_cached_parsed, FileHashIndex, ImporterIndex,
};
use crate::versioning::latest_graph_artifacts;

use super::{analyze_config_fingerprint, AnalyzeCacheOptions, AnalyzeConfigState, CacheStats};

pub(super) enum ParseScopeOutcome {
    Reused {
        artifacts: GraphArtifacts,
        parsed_files_path: PathBuf,
        node_count: usize,
        edge_count: usize,
        parsed_file_count: usize,
        cache_stats: CacheStats,
    },
    Parsed {
        parse_output: ParseOutput,
        current_hashes: HashMap<String, String>,
        cache_stats: CacheStats,
    },
}

pub(super) struct ReusedArtifacts {
    pub(super) artifacts: GraphArtifacts,
    pub(super) parsed_files_path: PathBuf,
    pub(super) node_count: usize,
    pub(super) edge_count: usize,
    pub(super) parsed_file_count: usize,
}

fn default_registry() -> cih_parse::LanguageRegistry {
    crate::scan::default_scan_registry()
}

pub(super) fn parse_scope(
    repo_root: &Path,
    cih_dir: &Path,
    files: &[String],
    cache: AnalyzeCacheOptions,
) -> Result<ParseScopeOutcome> {
    tracing::info!(
        total_files = files.len(),
        cache_enabled = cache.use_cache,
        "hashing files"
    );
    let current_hashes = hash_all(repo_root, files);
    tracing::debug!(hashed = current_hashes.len(), "file hashing complete");

    if !cache.use_cache {
        tracing::info!(files = files.len(), "cache disabled — parsing all files");
        let unit_output = cih_parse::parse_file_units(repo_root, files, &default_registry())?;
        for unit in &unit_output.units {
            if let Some(hash) = current_hashes.get(&unit.rel) {
                save_cached_parsed(cih_dir, hash, unit)?;
            }
        }
        let reparsed_files = unit_output.units.len();
        tracing::info!(
            reparsed = reparsed_files,
            skipped = unit_output.skipped.len(),
            "parse complete (no-cache)"
        );
        return Ok(ParseScopeOutcome::Parsed {
            parse_output: cih_parse::parse_output_from_units(
                unit_output.units,
                unit_output.skipped,
            ),
            current_hashes,
            cache_stats: CacheStats {
                enabled: false,
                reparsed_files,
                ..CacheStats::default()
            },
        });
    }

    let previous = FileHashIndex::load(cih_dir);
    let changed_files: Vec<String> = previous
        .changed_files(&current_hashes)
        .into_iter()
        .map(str::to_string)
        .collect();
    let all_files_hashed = current_hashes.len() == files.len();

    tracing::info!(
        changed = changed_files.len(),
        total = files.len(),
        "incremental cache check complete"
    );
    if !changed_files.is_empty() {
        for f in changed_files.iter().take(20) {
            tracing::debug!(file = %f, "changed");
        }
        if changed_files.len() > 20 {
            tracing::debug!("... and {} more changed files", changed_files.len() - 20);
        }
    }

    if cache.allow_noop
        && all_files_hashed
        && changed_files.is_empty()
        && previous.same_file_set(&current_hashes)
        && config_unchanged(repo_root, cih_dir, &cache)
    {
        match reused_artifacts(repo_root, cih_dir) {
            Ok(reused) => {
                tracing::info!("nothing changed, reusing last artifacts");
                return Ok(ParseScopeOutcome::Reused {
                    cache_stats: CacheStats {
                        enabled: true,
                        noop: true,
                        version: Some(reused.artifacts.version.to_string()),
                        ..CacheStats::default()
                    },
                    artifacts: reused.artifacts,
                    parsed_files_path: reused.parsed_files_path,
                    node_count: reused.node_count,
                    edge_count: reused.edge_count,
                    parsed_file_count: reused.parsed_file_count,
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, "incremental no-op unavailable; falling back to parse");
            }
        }
    }

    let mut cached_by_file: HashMap<String, ParsedUnit> = HashMap::new();
    for rel in files {
        let unit = current_hashes
            .get(rel)
            .and_then(|hash| load_cached_parsed(cih_dir, hash))
            .or_else(|| {
                previous
                    .get(rel)
                    .and_then(|hash| load_cached_parsed(cih_dir, hash))
            });
        if let Some(unit) = unit {
            cached_by_file.insert(rel.clone(), unit);
        }
    }

    let cached_parsed: Vec<_> = cached_by_file
        .values()
        .map(|unit| unit.parsed_file.clone())
        .collect();
    let importer_index = ImporterIndex::build(&cached_parsed);
    let mut to_parse: HashSet<String> = importer_index.expand(&changed_files, 4);
    for rel in files {
        if !cached_by_file.contains_key(rel) {
            to_parse.insert(rel.clone());
        }
        if !current_hashes.contains_key(rel) {
            to_parse.insert(rel.clone());
        }
    }

    let mut to_parse: Vec<String> = to_parse.into_iter().collect();
    to_parse.sort();

    let cache_hits_pre = files.len().saturating_sub(to_parse.len());
    tracing::info!(
        to_parse = to_parse.len(),
        cache_hits = cache_hits_pre,
        changed = changed_files.len(),
        "incremental parse: {} files to parse, {} from cache",
        to_parse.len(),
        cache_hits_pre,
    );

    let unit_output = cih_parse::parse_file_units(repo_root, &to_parse, &default_registry())?;
    tracing::info!(
        parsed = unit_output.units.len(),
        skipped = unit_output.skipped.len(),
        "parse units complete"
    );

    let reparsed_files = unit_output.units.len();
    let skipped_reparse: HashSet<&str> = unit_output
        .skipped
        .iter()
        .map(|skipped| skipped.rel.as_str())
        .collect();
    let mut parsed_by_file: HashMap<String, ParsedUnit> = HashMap::new();
    for unit in unit_output.units {
        if let Some(hash) = current_hashes.get(&unit.rel) {
            save_cached_parsed(cih_dir, hash, &unit)?;
        }
        parsed_by_file.insert(unit.rel.clone(), unit);
    }

    let mut combined_units = Vec::new();
    for rel in files {
        if let Some(unit) = parsed_by_file.remove(rel) {
            combined_units.push(unit);
        } else if !skipped_reparse.contains(rel.as_str()) {
            if let Some(unit) = cached_by_file.remove(rel) {
                combined_units.push(unit);
            }
        }
    }
    let cache_hits = combined_units
        .iter()
        .filter(|unit| !to_parse.iter().any(|rel| rel == &unit.rel))
        .count();
    let expanded_files = to_parse.len();

    Ok(ParseScopeOutcome::Parsed {
        parse_output: cih_parse::parse_output_from_units(combined_units, unit_output.skipped),
        current_hashes,
        cache_stats: CacheStats {
            enabled: true,
            noop: false,
            cache_hits,
            changed_files: changed_files.len(),
            expanded_files,
            reparsed_files,
            version: None,
        },
    })
}

/// Whether the output-affecting analyze config still matches what produced the cached artifacts.
/// A missing or differing fingerprint (e.g. a new `--cxf-base-path` or edited `cih.patterns.toml`)
/// returns `false`, which disqualifies the no-op reuse so resolve/post-process/emit re-run.
fn config_unchanged(repo_root: &Path, cih_dir: &Path, cache: &AnalyzeCacheOptions) -> bool {
    let current = analyze_config_fingerprint(repo_root, cache);
    match AnalyzeConfigState::load(cih_dir) {
        Some(prev) if prev.fingerprint == current => true,
        _ => {
            tracing::info!("analyze config changed since last run — re-resolving (no source changes)");
            false
        }
    }
}

fn reused_artifacts(repo_root: &Path, cih_dir: &Path) -> Result<ReusedArtifacts> {
    let artifacts = latest_graph_artifacts(repo_root)?;
    let nodes = artifacts.read_nodes()?;
    let edges = artifacts.read_edges()?;
    let parsed_dir = cih_dir.join("parsed").join(artifacts.version.as_str());
    let parsed_files_path = parsed_dir.join("parsed-files.jsonl");
    let parsed_file_count = cih_parse::load_parsed_files(&parsed_dir)
        .map(|files| files.len())
        .unwrap_or(0);
    Ok(ReusedArtifacts {
        artifacts,
        parsed_files_path,
        node_count: nodes.len(),
        edge_count: edges.len(),
        parsed_file_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("cih-cfgfp-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn opts(cxf: Option<&str>, skip_xml: bool) -> AnalyzeCacheOptions {
        AnalyzeCacheOptions {
            use_cache: true,
            allow_noop: true,
            skip_xml_integration: skip_xml,
            cxf_base_path: cxf.map(String::from),
        }
    }

    /// Persist the fingerprint for `cache`, exactly as the emit path does.
    fn record(repo: &Path, cih_dir: &Path, cache: &AnalyzeCacheOptions) {
        AnalyzeConfigState {
            fingerprint: analyze_config_fingerprint(repo, cache),
        }
        .save(cih_dir)
        .unwrap();
    }

    #[test]
    fn unchanged_config_matches_and_changed_config_does_not() {
        let repo = temp_repo("cxf");
        let cih_dir = repo.join(".cih");
        let a = opts(None, false);
        record(&repo, &cih_dir, &a);

        // Identical config → reuse allowed.
        assert!(config_unchanged(&repo, &cih_dir, &a));
        // A new --cxf-base-path with no source change → reuse disqualified.
        let b = opts(Some("/cxf"), false);
        assert!(!config_unchanged(&repo, &cih_dir, &b));
        // Toggling skip_xml_integration also invalidates.
        assert!(!config_unchanged(&repo, &cih_dir, &opts(None, true)));

        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn missing_fingerprint_disqualifies_reuse() {
        let repo = temp_repo("missing");
        let cih_dir = repo.join(".cih"); // never written
        assert!(!config_unchanged(&repo, &cih_dir, &opts(None, false)));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn editing_patterns_file_invalidates_reuse() {
        let repo = temp_repo("patterns");
        let cih_dir = repo.join(".cih");
        let cache = opts(None, false);
        // Fingerprint recorded with no cih.patterns.toml present.
        record(&repo, &cih_dir, &cache);
        assert!(config_unchanged(&repo, &cih_dir, &cache));

        // Adding a resolve pattern changes the effective config even though no source file did.
        std::fs::write(
            repo.join(cih_patterns::PATTERNS_FILE),
            "[[route]]\nannotation = \"BankEndpoint\"\nmethod = \"POST\"\n",
        )
        .unwrap();
        assert!(!config_unchanged(&repo, &cih_dir, &cache));

        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn fingerprint_is_deterministic_and_ignores_cache_control() {
        let repo = temp_repo("det");
        let mut a = opts(Some("/rest"), false);
        let fp1 = analyze_config_fingerprint(&repo, &a);
        assert_eq!(fp1, analyze_config_fingerprint(&repo, &a));
        // Flipping cache-control fields must not change the fingerprint (they don't affect output).
        a.use_cache = false;
        a.allow_noop = false;
        assert_eq!(fp1, analyze_config_fingerprint(&repo, &a));
        std::fs::remove_dir_all(&repo).ok();
    }
}
