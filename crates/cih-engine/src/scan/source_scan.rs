//! Registry-driven per-file source scan: LOC (newline count, no parse),
//! package/namespace, and framework signal extraction. Parallel via rayon.
//! Replaces the old Java-only `java_scan.rs` — now dispatches through
//! `LanguageProvider::scan_file` for all registered languages.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use cih_parse::LanguageRegistry;
use rayon::prelude::*;

use super::{ScannedFile, SourceFileInfo};

pub fn collect_source_files(
    root: &Path,
    files: &[ScannedFile],
    registry: &LanguageRegistry,
) -> Vec<SourceFileInfo> {
    let source_count = files
        .iter()
        .filter(|f| registry.provider_for(&f.path).is_some())
        .count();
    tracing::debug!(
        source_files = source_count,
        "source_scan: starting per-file LOC/package/framework extraction"
    );

    let result: Vec<SourceFileInfo> = files
        .par_iter()
        .filter_map(|file| {
            let provider = registry.provider_for(&file.path)?;
            let full_path = root.join(&file.path);
            let content = fs::read_to_string(&full_path).ok()?;
            let scan = provider.scan_file(&file.path, &content).ok()?;
            tracing::debug!(
                file = %file.path,
                language = provider.language_id(),
                loc = scan.loc,
                frameworks = ?scan.frameworks,
                "source_scan: parsed file"
            );
            Some(SourceFileInfo {
                path: file.path.clone(),
                language: provider.language_id().to_string(),
                loc: scan.loc,
                package: scan.package,
                frameworks: scan.frameworks,
            })
        })
        .collect();

    let framework_files = result.iter().filter(|f| !f.frameworks.is_empty()).count();
    tracing::debug!(
        parsed = result.len(),
        framework_annotated = framework_files,
        "source_scan: extraction complete"
    );
    result
}

pub fn collect_decompiled_dirs(files: &[ScannedFile]) -> Vec<String> {
    let mut dirs = BTreeSet::new();
    for file in files {
        if file.path == ".workspace-dependencies"
            || file.path.starts_with(".workspace-dependencies/")
        {
            dirs.insert(".workspace-dependencies".to_string());
        }
    }
    dirs.into_iter().collect()
}
