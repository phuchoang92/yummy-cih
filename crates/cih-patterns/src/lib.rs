//! User-defined *resolve patterns* — teach CIH a codebase's own framework conventions without
//! shipping a new hardcoded handler each time.
//!
//! A per-repo `cih.patterns.toml` (modeled on `cih.taint.toml`) declares rules that a generic,
//! deterministic pass applies over the assembled graph. Rules match the annotation metadata that
//! the parser now retains on every node, so a custom `@BankEndpoint("/pay")` becomes a real
//! `Route` with no framework-specific Rust. Loading is fail-soft: a missing/malformed file yields
//! empty rules and never panics.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The filename read at a repository root.
pub const PATTERNS_FILE: &str = "cih.patterns.toml";

/// A rule that turns a method carrying a custom annotation into an HTTP `Route`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRule {
    /// Simple annotation name to match on a method (e.g. `"BankEndpoint"`). No `@`.
    pub annotation: String,
    /// Annotation attribute holding the URL path. Defaults to `"value"` (the positional arg).
    #[serde(default = "default_value_attr", skip_serializing_if = "is_value_attr")]
    pub path_attr: String,
    /// Fixed HTTP method for every match (e.g. `"POST"`). Use this or `method_attr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Annotation attribute holding the HTTP method, when it varies per usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method_attr: Option<String>,
    /// Optional class-level annotation whose value is a path prefix (e.g. `"BankResource"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_prefix_annotation: Option<String>,
    /// Attribute on the class-level annotation holding the prefix. Defaults to `"value"`.
    #[serde(default = "default_value_attr", skip_serializing_if = "is_value_attr")]
    pub class_prefix_attr: String,
}

impl RouteRule {
    /// The HTTP method a match should use: the fixed `method`, else uppercased `GET` fallback is
    /// left to the applier; here we just expose what was declared.
    pub fn fixed_method(&self) -> Option<String> {
        self.method.as_ref().map(|m| m.to_ascii_uppercase())
    }
}

/// All user-declared resolve patterns for a repo.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternRules {
    #[serde(default, rename = "route", skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<RouteRule>,
}

impl PatternRules {
    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    /// Add a route rule, de-duplicating an identical one. Returns `true` when newly added.
    pub fn add_route(&mut self, rule: RouteRule) -> bool {
        if self.routes.contains(&rule) {
            return false;
        }
        self.routes.push(rule);
        true
    }
}

/// `<repo>/cih.patterns.toml`.
pub fn patterns_path(repo: &Path) -> PathBuf {
    repo.join(PATTERNS_FILE)
}

/// Load resolve patterns for `repo`. Fail-soft: a missing file yields empty rules; a malformed
/// file logs a warning and yields empty rules (built-in detectors always still run).
pub fn load_patterns(repo: &Path) -> PatternRules {
    let path = patterns_path(repo);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return PatternRules::default(),
    };
    parse_patterns(&content).unwrap_or_else(|e| {
        tracing::warn!(path = %path.display(), err = %e, "invalid cih.patterns.toml — ignoring");
        PatternRules::default()
    })
}

/// Parse patterns from a TOML string (separated for testability).
pub fn parse_patterns(content: &str) -> Result<PatternRules, toml::de::Error> {
    toml::from_str(content)
}

/// Serialize patterns to the TOML written at the repo root.
pub fn to_toml(rules: &PatternRules) -> String {
    toml::to_string_pretty(rules).unwrap_or_default()
}

fn default_value_attr() -> String {
    "value".to_string()
}

fn is_value_attr(s: &String) -> bool {
    s == "value"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_route_rules() {
        let toml = r#"
            [[route]]
            annotation = "BankEndpoint"
            method = "POST"

            [[route]]
            annotation = "BankQuery"
            path_attr = "url"
            method_attr = "verb"
            class_prefix_annotation = "BankResource"
        "#;
        let rules = parse_patterns(toml).unwrap();
        assert_eq!(rules.routes.len(), 2);
        assert_eq!(rules.routes[0].annotation, "BankEndpoint");
        assert_eq!(rules.routes[0].path_attr, "value"); // default
        assert_eq!(rules.routes[0].fixed_method().as_deref(), Some("POST"));
        assert_eq!(rules.routes[1].path_attr, "url");
        assert_eq!(rules.routes[1].method_attr.as_deref(), Some("verb"));
        assert_eq!(rules.routes[1].class_prefix_annotation.as_deref(), Some("BankResource"));
    }

    #[test]
    fn malformed_toml_is_an_error() {
        assert!(parse_patterns("[[route]]\nannotation = ").is_err());
    }

    #[test]
    fn add_route_dedupes() {
        let mut rules = PatternRules::default();
        let r = RouteRule {
            annotation: "X".into(),
            path_attr: "value".into(),
            method: Some("GET".into()),
            method_attr: None,
            class_prefix_annotation: None,
            class_prefix_attr: "value".into(),
        };
        assert!(rules.add_route(r.clone()));
        assert!(!rules.add_route(r)); // duplicate
        assert_eq!(rules.routes.len(), 1);
    }

    #[test]
    fn load_and_write_on_disk() {
        let dir = std::env::temp_dir().join(format!("cih-patterns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // missing file → empty (fail-soft)
        assert!(load_patterns(&dir).is_empty());
        // write via to_toml, read back via load_patterns
        let mut rules = PatternRules::default();
        rules.add_route(RouteRule {
            annotation: "BankEndpoint".into(),
            path_attr: "value".into(),
            method: Some("POST".into()),
            method_attr: None,
            class_prefix_annotation: None,
            class_prefix_attr: "value".into(),
        });
        std::fs::write(patterns_path(&dir), to_toml(&rules)).unwrap();
        let loaded = load_patterns(&dir);
        assert_eq!(loaded, rules);
        // malformed → empty (fail-soft, no panic)
        std::fs::write(patterns_path(&dir), "[[route]]\nannotation =").unwrap();
        assert!(load_patterns(&dir).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roundtrips_through_toml() {
        let mut rules = PatternRules::default();
        rules.add_route(RouteRule {
            annotation: "BankEndpoint".into(),
            path_attr: "value".into(),
            method: Some("POST".into()),
            method_attr: None,
            class_prefix_annotation: None,
            class_prefix_attr: "value".into(),
        });
        let text = to_toml(&rules);
        let back = parse_patterns(&text).unwrap();
        assert_eq!(back, rules);
    }
}
