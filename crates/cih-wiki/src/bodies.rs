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
                    stripped: redact_secrets(&stripped),
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

/// Replace credential-shaped substrings with a typed placeholder.
/// Covers the highest-risk patterns: JDBC connection strings, PEM blocks,
/// cloud provider keys, and generic bearer/password assignments.
pub fn redact_secrets(src: &str) -> String {
    let mut out = src.to_string();

    // PEM blocks (multi-line: match header and collapse body to placeholder)
    // Replace everything between -----BEGIN ... ----- and -----END ... -----
    while let Some(start) = out.find("-----BEGIN ") {
        if let Some(end) = out[start..].find("-----END ") {
            if let Some(end2) = out[start + end..].find("-----") {
                let full_end = start + end + end2 + 5;
                let header_end = out[start..].find('\n').map(|i| start + i + 1).unwrap_or(start);
                let label = out[start..header_end.min(start + 40)].trim_end();
                let tag = format!("[REDACTED:pem:{}]", label.trim_start_matches('-').trim_end_matches('-').trim());
                out.replace_range(start..full_end, &tag);
                continue;
            }
        }
        break;
    }

    // JDBC connection strings: jdbc:...:password=...
    out = redact_pattern_value(
        &out,
        &["password=", "passwd=", "pwd="],
        ";& \t\n\"'",
        "REDACTED:jdbc",
    );

    // Generic assignment patterns: password = "...", apiKey: "...", secret = '...'
    out = redact_quoted_assignment(
        &out,
        &["password", "passwd", "secret", "api_key", "apikey", "apiSecret",
          "access_key", "access_token", "private_key", "client_secret"],
        "REDACTED:cred",
    );

    // AWS key patterns: AKIA..., ASIA...
    out = redact_regex_like(&out, "AKIA[A-Z0-9]{16}", "[REDACTED:aws-key]");
    out = redact_regex_like(&out, "ASIA[A-Z0-9]{16}", "[REDACTED:aws-key]");

    out
}

fn redact_pattern_value(src: &str, patterns: &[&str], terminators: &str, tag: &str) -> String {
    let mut out = src.to_string();
    for pat in patterns {
        let pat_lower = pat.to_lowercase();
        let mut pos = 0;
        while pos < out.len() {
            let haystack = out[pos..].to_lowercase();
            let Some(idx) = haystack.find(pat_lower.as_str()) else { break };
            let abs = pos + idx + pat.len();
            // find end of value (next terminator or end of string)
            let end = out[abs..].find(|c: char| terminators.contains(c))
                .map(|i| abs + i)
                .unwrap_or(out.len());
            let value = out[abs..end].trim_matches(|c| c == '"' || c == '\'' || c == '`');
            if value.len() >= 4 {
                let replacement = format!("{}[{}]", &pat, tag);
                out.replace_range(abs..end, &format!("[{}]", tag));
                pos = abs + replacement.len();
            } else {
                pos = abs;
            }
        }
    }
    out
}

fn redact_quoted_assignment(src: &str, keywords: &[&str], tag: &str) -> String {
    let mut out = src.to_string();
    for kw in keywords {
        let kw_lower = kw.to_lowercase();
        let mut pos = 0;
        while pos < out.len() {
            let haystack = out[pos..].to_lowercase();
            let Some(idx) = haystack.find(kw_lower.as_str()) else { break };
            let after_kw = pos + idx + kw.len();
            // skip whitespace, :, =, spaces
            let rest = out[after_kw..].trim_start_matches([' ', '\t', ':', '=', ' ']);
            let rest_start = after_kw + (out[after_kw..].len() - rest.len());
            // must be followed by a quoted string
            if let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') {
                let value_start = rest_start + 1;
                if let Some(close) = out[value_start..].find(quote) {
                    let value_end = value_start + close;
                    let value = &out[value_start..value_end];
                    if value.len() >= 4 {
                        let replacement = format!("[{}]", tag);
                        out.replace_range(value_start..value_end, &replacement);
                        pos = value_start + replacement.len() + 1;
                        continue;
                    }
                }
            }
            pos = after_kw;
        }
    }
    out
}

/// Redacts fixed-pattern tokens matching a simple character-class rule (no regex crate needed).
fn redact_regex_like(src: &str, pattern_prefix: &str, replacement: &str) -> String {
    // pattern_prefix is "AKIA" or "ASIA"; we scan for it followed by 16 uppercase alphanum chars
    let prefix = &pattern_prefix[..4];
    let suffix_len = 16;
    let mut out = src.to_string();
    let mut pos = 0;
    while pos + 4 + suffix_len <= out.len() {
        if let Some(idx) = out[pos..].find(prefix) {
            let abs = pos + idx;
            let suffix = &out[abs + 4..abs + 4 + suffix_len];
            if suffix.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
                out.replace_range(abs..abs + 4 + suffix_len, replacement);
                pos = abs + replacement.len();
            } else {
                pos = abs + 4;
            }
        } else {
            break;
        }
    }
    out
}
