use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use cih_core::{GraphArtifacts, ParsedUnit};
use cih_parse::ParseOutput;

use crate::file_cache::{
    hash_all, load_cached_parsed, save_cached_parsed, FileHashIndex, ImporterIndex,
};
use crate::versioning::latest_graph_artifacts;

use super::{AnalyzeCacheOptions, CacheStats};

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
    {
        match reused_artifacts(repo_root, cih_dir) {
            Ok(reused) => {
                tracing::info!("nothing changed, reusing last artifacts");
                return Ok(ParseScopeOutcome::Reused {
                    cache_stats: CacheStats {
                        enabled: true,
                        noop: true,
                        version: Some(reused.artifacts.version.0.clone()),
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

fn reused_artifacts(repo_root: &Path, cih_dir: &Path) -> Result<ReusedArtifacts> {
    let artifacts = latest_graph_artifacts(repo_root)?;
    let nodes = artifacts.read_nodes()?;
    let edges = artifacts.read_edges()?;
    let parsed_dir = cih_dir.join("parsed").join(&artifacts.version.0);
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
