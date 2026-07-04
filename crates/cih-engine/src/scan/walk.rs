//! Gitignore-aware filesystem walk to `ScannedFile` paths + sizes (no content).
//! Mirrors GitNexus's `filesystem-walker.ts`, using the `ignore` crate.

use std::path::Path;

use anyhow::Result;
use ignore::{DirEntry, WalkBuilder};

use super::ignore_rules::{should_ignore_dir, should_ignore_path};
use super::paths::rel_path;
use super::ScannedFile;

pub(super) fn walk_repository_paths(root: &Path) -> Result<Vec<ScannedFile>> {
    tracing::debug!(root = %root.display(), "walk: starting gitignore-aware filesystem walk");

    let root_for_filter = root.to_path_buf();
    let root_for_log = root.to_path_buf();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .add_custom_ignore_filename(".cihignore")
        .filter_entry(move |entry| {
            let rel = rel_path(&root_for_filter, entry.path());
            if !rel.is_empty()
                && entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
                && should_ignore_dir(&rel)
            {
                tracing::debug!(dir = %rel, "walk: skipping ignored directory");
                return false;
            }
            should_descend_or_keep(&root_for_filter, entry)
        });

    let mut files = Vec::new();
    for result in builder.build() {
        let entry = result?;
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let rel = rel_path(root, entry.path());
        if should_ignore_path(&rel) {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        files.push(ScannedFile { path: rel, size });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    tracing::debug!(
        root = %root_for_log.display(),
        files = files.len(),
        total_bytes,
        "walk: filesystem walk complete"
    );
    Ok(files)
}

fn should_descend_or_keep(root: &Path, entry: &DirEntry) -> bool {
    let rel = rel_path(root, entry.path());
    if rel.is_empty() {
        return true;
    }
    if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
        !should_ignore_dir(&rel)
    } else {
        !should_ignore_path(&rel)
    }
}
