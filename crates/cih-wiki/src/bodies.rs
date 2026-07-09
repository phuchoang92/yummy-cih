use std::collections::HashMap;
use std::path::Path;

use cih_core::{Node, NodeKind};

pub struct BodyEntry {
    pub stripped: String,
    /// Raw source line count before stripping (end_line - start_line + 1).
    pub original_lines: usize,
}

fn is_body_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Method | NodeKind::Constructor | NodeKind::Function
    )
}

fn file_ext(file: &str) -> &str {
    file.rfind('.').map(|i| &file[i + 1..]).unwrap_or("")
}

/// Build a node_id → stripped body map by reading source files from `repo`.
/// Only Method/Constructor/Function nodes with valid line ranges get a body entry.
pub fn source_bodies(nodes: &[Node], repo: &Path) -> HashMap<String, BodyEntry> {
    let mut file_lines: HashMap<String, Vec<String>> = HashMap::new();
    let mut bodies: HashMap<String, BodyEntry> = HashMap::new();

    for node in nodes {
        if !is_body_kind(node.kind) {
            continue;
        }
        let start = node.range.start_line as usize;
        let end = node.range.end_line as usize;
        if start == 0 && end == 0 {
            continue;
        }

        let lines = file_lines.entry(node.file.clone()).or_insert_with(|| {
            std::fs::read_to_string(repo.join(&node.file))
                .unwrap_or_default()
                .lines()
                .map(|l| l.to_string())
                .collect()
        });
        if lines.is_empty() {
            continue;
        }

        let from = start.saturating_sub(1);
        let to = end.min(lines.len());
        if from >= to {
            continue;
        }

        let original_lines = to - from;
        let raw = lines[from..to].join("\n");
        let stripped = match file_ext(&node.file) {
            "java" => strip_java_body(&raw),
            _ => raw,
        };

        if !stripped.trim().is_empty() {
            bodies.insert(
                node.id.as_str().to_string(),
                BodyEntry {
                    stripped,
                    original_lines,
                },
            );
        }
    }

    bodies
}

fn strip_java_body(src: &str) -> String {
    src.lines()
        .filter(|line| !is_noise_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_noise_line(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return false;
    }
    if is_log_call(t) {
        return true;
    }
    if is_null_guard(t) {
        return true;
    }
    if t.starts_with("super(") && t.ends_with(");") {
        return true;
    }
    if is_trivial_getter_body(t) {
        return true;
    }
    false
}

fn is_log_call(t: &str) -> bool {
    let prefixes = ["log.", "logger.", "LOG.", "LOGGER."];
    prefixes.iter().any(|p| t.starts_with(p))
        || ((t.contains(".debug(")
            || t.contains(".info(")
            || t.contains(".warn(")
            || t.contains(".error("))
            && (t.contains("log.")
                || t.contains("logger.")
                || t.contains("LOG.")
                || t.contains("LOGGER.")))
}

fn is_null_guard(t: &str) -> bool {
    (t.starts_with("if (") || t.starts_with("if(")) && t.contains("== null") && t.contains("throw")
}

fn is_trivial_getter_body(t: &str) -> bool {
    (t.starts_with("return this.") && t.ends_with(';') && !t.contains('(')) || t == "return this;"
}
