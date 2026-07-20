use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).expect("architecture source directory must be readable") {
            let path = entry.expect("source entry must be readable").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                files.push(path);
            }
        }
    }
    files
}

fn production_source(path: &Path) -> String {
    let source = fs::read_to_string(path).expect("Rust source must be readable");
    source
        .split("\n#[cfg(test)]")
        .next()
        .unwrap_or(&source)
        .to_string()
}

fn assert_absent(layer: &str, forbidden: &[&str]) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(layer);
    for path in rust_files(&root) {
        let source = production_source(&path);
        for needle in forbidden {
            assert!(
                !source.contains(needle),
                "{} production code must not depend on `{needle}`: {}",
                layer,
                path.display()
            );
        }
    }
}

#[test]
fn domain_has_no_inward_or_transport_dependencies() {
    assert_absent(
        "domain",
        &[
            "rmcp",
            "axum",
            "crate::application",
            "crate::infrastructure",
            "crate::ports",
            "crate::transport",
        ],
    );
}

#[test]
fn application_depends_on_domain_and_ports_not_adapters() {
    assert_absent(
        "application",
        &["rmcp", "axum", "crate::infrastructure", "crate::transport"],
    );
}

#[test]
fn ports_do_not_depend_on_adapters_or_transports() {
    assert_absent(
        "ports",
        &["rmcp", "axum", "crate::infrastructure", "crate::transport"],
    );
}
