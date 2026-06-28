use cih_core::NodeKind;

use crate::config::PackageConfig;
use crate::entry::{fnv64_node, FeatureGroupEntry};
use crate::strategy::{FeatureStrategy, StrategyInput};

/// Rule-based feature classifier derived from Java/Kotlin file paths.
///
/// Three-strategy cascade (in order):
/// 1. `modules/<feature>/` explicit segment in the path
/// 2. Maven multi-module directory name before `/src/main/java/` (normalised)
/// 3. Deepest meaningful segment of the Java package path (walking right to left)
///
/// Falls back to `"shared"` when no strategy yields a domain-specific name.
pub struct PackageStrategy {
    config: PackageConfig,
}

impl PackageStrategy {
    pub fn new(config: PackageConfig) -> Self {
        PackageStrategy { config }
    }

    // ── private helpers ────────────────────────────────────────────────────

    /// Core three-strategy classifier (shared by `feature_of` and `assign`).
    fn classify(&self, file: &str) -> (String, String) {
        // Strategy 1: explicit modules/<feature>/ segment
        if let Some(start) = file.find("modules/") {
            let rest = &file[start + "modules/".len()..];
            if let Some(end) = rest.find('/') {
                if end > 0 {
                    let feat = rest[..end].to_string();
                    return (feat.clone(), format!("modules/{}/", feat));
                }
            }
        }

        // Normalize path to always have a leading slash so src_root markers (which all
        // start with '/') can match root-relative paths like "src/main/java/…".
        let normalized;
        let file = if file.starts_with('/') {
            file
        } else {
            normalized = format!("/{}", file);
            normalized.as_str()
        };

        // Strategy 2: Maven multi-module directory before src root marker
        if let Some(marker_pos) = self.config.src_roots.iter().find_map(|m| file.find(m.as_str())) {
            let module_dir = &file[..marker_pos];
            let module_name = module_dir.rsplit('/').next().unwrap_or(module_dir);
            if !module_name.is_empty() {
                let normalised = self.normalise_module_name(module_name);
                if !normalised.is_empty()
                    && normalised != "shared"
                    && !self.config.catch_all.iter().any(|c| c == &normalised)
                {
                    return (
                        normalised.clone(),
                        format!("Maven module {} → {}", module_name, normalised),
                    );
                }
            }

            // Strategy 3: deepest meaningful segment of the Java package path
            for marker in &self.config.src_roots {
                if let Some(pos) = file.find(marker.as_str()) {
                    let pkg_path = &file[pos + marker.len()..];
                    let pkg_dir = match pkg_path.rfind('/') {
                        Some(p) => &pkg_path[..p],
                        None => pkg_path,
                    };
                    if let Some(feat) = self.meaningful_package_feature(pkg_dir) {
                        return (feat.clone(), format!("Java package {}", pkg_dir));
                    }
                    break;
                }
            }
        }

        ("shared".to_string(), "no domain segment found".to_string())
    }

    fn normalise_module_name(&self, name: &str) -> String {
        let mut s = name.to_lowercase();

        let mut changed = true;
        while changed {
            changed = false;
            for suf in &self.config.strip_suffixes {
                if s.len() > suf.len() && s.ends_with(suf.as_str()) {
                    s.truncate(s.len() - suf.len());
                    changed = true;
                }
            }
        }
        changed = true;
        while changed {
            changed = false;
            for pfx in &self.config.strip_prefixes {
                if s.len() > pfx.len() && s.starts_with(pfx.as_str()) {
                    s = s[pfx.len()..].to_string();
                    changed = true;
                }
            }
        }

        if s.len() <= 1 || s.chars().all(|c| c.is_ascii_digit()) {
            return String::new();
        }
        s
    }

    fn meaningful_package_feature(&self, pkg_dir: &str) -> Option<String> {
        for segment in pkg_dir.split('/').rev() {
            let seg = segment.trim();
            if seg.is_empty() || seg.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if self.config.skip_segments.iter().any(|s| s == seg) {
                continue;
            }
            if seg.len() <= 1 {
                continue;
            }
            let mut chars = seg.chars();
            if matches!(chars.next(), Some('v') | Some('V')) && chars.all(|c| c.is_ascii_digit()) {
                continue;
            }
            return Some(seg.to_string());
        }
        None
    }

}

impl FeatureStrategy for PackageStrategy {
    fn name(&self) -> &str {
        "package"
    }

    fn feature_of(&self, file: &str) -> String {
        self.classify(file).0
    }

    fn assign(&self, input: &StrategyInput<'_>) -> Vec<FeatureGroupEntry> {
        input
            .nodes
            .iter()
            .filter(|n| {
                matches!(
                    n.kind,
                    NodeKind::Class
                        | NodeKind::Interface
                        | NodeKind::Enum
                        | NodeKind::Record
                        | NodeKind::Annotation
                        | NodeKind::Method
                        | NodeKind::Function
                        | NodeKind::Constructor
                )
            })
            .map(|n| {
                let (feat, evidence) = self.classify(&n.file);
                FeatureGroupEntry {
                    id: format!("feature:{}", feat),
                    name: feat,
                    node_id: n.id.as_str().to_string(),
                    strategy: "package".to_string(),
                    confidence: 1.0,
                    pinned: false,
                    evidence,
                    node_content_hash: fnv64_node(n),
                }
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "package_tests.rs"]
mod tests;
