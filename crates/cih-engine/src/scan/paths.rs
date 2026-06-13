//! Relative-path helpers shared by the scan submodules.

use std::path::{Path, PathBuf};

pub(super) fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(super) fn normalize_path(path: PathBuf) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub(super) fn parent_rel(rel: &str) -> String {
    rel.rsplit_once('/')
        .map(|(parent, _)| {
            if parent.is_empty() {
                ".".to_string()
            } else {
                parent.to_string()
            }
        })
        .unwrap_or_else(|| ".".to_string())
}

pub(super) fn join_rel(base: &str, child: &str) -> String {
    let child = child.trim_matches('/');
    if base == "." || base.is_empty() {
        child.to_string()
    } else if child.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{child}")
    }
}

pub(super) fn path_from_rel(rel: &str) -> PathBuf {
    if rel == "." {
        PathBuf::new()
    } else {
        PathBuf::from(rel)
    }
}
