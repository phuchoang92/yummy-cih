use std::collections::HashMap;
use std::path::Path;

const GENERIC_DIRS: &[&str] = &[
    "src",
    "lib",
    "core",
    "utils",
    "common",
    "shared",
    "helpers",
    "java",
    "main",
    "kotlin",
    "resources",
    "test",
];

pub fn heuristic_label(member_file_paths: &[&str], comm_idx: usize) -> String {
    if let Some(label) = folder_label(member_file_paths) {
        return label;
    }
    if let Some(label) = prefix_label(member_file_paths) {
        return label;
    }
    format!("Cluster_{comm_idx}")
}

fn folder_label(paths: &[&str]) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for path in paths {
        let Some(parent) = Path::new(path).parent() else {
            continue;
        };
        for component in parent.components().rev() {
            let token = component.as_os_str().to_string_lossy().to_string();
            let lower = token.to_ascii_lowercase();
            if !GENERIC_DIRS.contains(&lower.as_str()) && !token.trim().is_empty() {
                *counts.entry(token).or_default() += 1;
                break;
            }
        }
    }
    counts
        .into_iter()
        .max_by(|(a, ac), (b, bc)| ac.cmp(bc).then_with(|| b.cmp(a)))
        .map(|(token, _)| capitalize(&token))
}

fn prefix_label(paths: &[&str]) -> Option<String> {
    let mut stems: Vec<String> = paths
        .iter()
        .filter_map(|p| Path::new(p).file_stem())
        .map(|s| s.to_string_lossy().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    stems.sort();
    stems.dedup();
    if stems.len() < 3 {
        return None;
    }

    let mut best = String::new();
    for i in 0..stems.len() {
        for j in (i + 2)..stems.len() {
            let prefix = common_prefix(&stems[i], &stems[j]);
            if prefix.len() > best.len()
                && prefix.len() > 2
                && stems.iter().filter(|s| s.starts_with(&prefix)).count() >= 3
            {
                best = prefix;
            }
        }
    }
    (!best.is_empty()).then(|| capitalize(best.trim_matches('_')))
}

fn common_prefix(a: &str, b: &str) -> String {
    a.chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
