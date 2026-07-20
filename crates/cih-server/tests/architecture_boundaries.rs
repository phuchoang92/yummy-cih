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

/// Strip `#[cfg(test)]` sections, keeping ALL production code.
///
/// This used to `split(...).next()`, which discarded everything after the
/// *first* `#[cfg(test)]` — in `application/architecture_overview.rs` that is a
/// test-only `const` around line 176 of ~2200, so the bulk of the largest
/// application module was never scanned and could have imported anything.
///
/// A `#[cfg(test)]` attribute at column 0 introduces either a test module (whose
/// body ends at the matching column-0 `}`) or a single test-only item. Skipping
/// to the end of a braced block and resuming keeps every later production item
/// under inspection.
fn production_source(path: &Path) -> String {
    let source = fs::read_to_string(path).expect("Rust source must be readable");
    let mut production = String::with_capacity(source.len());
    let mut lines = source.lines();
    while let Some(line) = lines.next() {
        if line != "#[cfg(test)]" {
            production.push_str(line);
            production.push('\n');
            continue;
        }
        // Consume the attributed item. A block item ends at the first column-0
        // `}`; a non-block item ends at its first line.
        let mut in_block = false;
        for body in lines.by_ref() {
            if body.contains('{') {
                in_block = true;
            }
            if in_block {
                if body == "}" {
                    break;
                }
            } else {
                break;
            }
        }
    }
    production
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

/// The stripper itself is load-bearing: when it stopped at the first
/// `#[cfg(test)]`, everything after it was unguarded. This pins that production
/// code on both sides of test items stays under inspection, and that test code
/// does not produce false positives.
#[test]
fn production_source_keeps_code_after_test_items() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sample.rs");
    fs::write(
        &path,
        "use crate::domain::a;\n\
         #[cfg(test)]\n\
         const FIXTURE: &str = \"crate::transport::forbidden_in_test_const\";\n\
         use crate::ports::b;\n\
         #[cfg(test)]\n\
         mod tests {\n\
         use crate::infrastructure::forbidden_in_test_mod;\n\
         fn nested() {\n\
         }\n\
         }\n\
         use crate::domain::c;\n",
    )
    .expect("write sample");

    let production = production_source(&path);
    // Production code before, between, and after test items survives.
    assert!(production.contains("crate::domain::a"));
    assert!(production.contains("crate::ports::b"));
    assert!(
        production.contains("crate::domain::c"),
        "code after a test module was dropped — the guard would be blind here"
    );
    // Test-only code is excluded, including a braced module with nested braces.
    assert!(!production.contains("forbidden_in_test_const"));
    assert!(!production.contains("forbidden_in_test_mod"));
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
