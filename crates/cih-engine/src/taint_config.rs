//! Load taint rules from an optional `cih.taint.toml` in the repo root.
//!
//! If the file is absent or unparseable, `default_rules()` is returned unchanged.
//! If present, user-specified sinks and sanitizers are merged with the defaults
//! (or replace them entirely if `settings.extend_defaults = false`).

use cih_taint::{default_rules, SinkCategory, TaintRules, TaintSanitizer, TaintSink};

#[derive(serde::Deserialize, Default)]
struct TomlRules {
    #[serde(default)]
    sink: Vec<TomlSink>,
    #[serde(default)]
    sanitizer: Vec<TomlSanitizer>,
    #[serde(default)]
    settings: TomlSettings,
}

#[derive(serde::Deserialize)]
struct TomlSink {
    pattern: String,
    /// "sql" | "exec" | "file" | "html" — defaults to "sql"
    #[serde(default)]
    category: Option<String>,
}

#[derive(serde::Deserialize)]
struct TomlSanitizer {
    pattern: String,
}

#[derive(serde::Deserialize)]
struct TomlSettings {
    /// true = merge with built-ins; false = replace entirely (advanced use)
    #[serde(default = "default_true")]
    extend_defaults: bool,
}

impl Default for TomlSettings {
    fn default() -> Self {
        Self { extend_defaults: true }
    }
}

fn default_true() -> bool {
    true
}

/// Load taint rules for `repo`. Returns merged rules (user + defaults unless
/// `extend_defaults = false`). Falls back to `default_rules()` on any error.
pub(crate) fn load_taint_rules(repo: &std::path::Path) -> TaintRules {
    let config_path = repo.join("cih.taint.toml");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return default_rules(),
    };
    let parsed: TomlRules = match toml::from_str(&content) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                path = %config_path.display(),
                err = %e,
                "invalid cih.taint.toml — using default taint rules"
            );
            return default_rules();
        }
    };

    let base = if parsed.settings.extend_defaults {
        default_rules()
    } else {
        TaintRules {
            sinks: vec![],
            sanitizers: vec![],
            extra_sink_name_patterns: vec![],
            max_hops: 12,
        }
    };

    let user_sinks: Vec<TaintSink> = parsed
        .sink
        .iter()
        .map(|s| {
            let category = match s.category.as_deref() {
                Some("exec") => SinkCategory::Exec,
                Some("file") => SinkCategory::File,
                Some("html") => SinkCategory::Html,
                _ => SinkCategory::Sql,
            };
            TaintSink { node_id_pattern: s.pattern.clone(), category }
        })
        .collect();

    let user_extra: Vec<String> = parsed
        .sink
        .iter()
        .map(|s| {
            let p = s.pattern.as_str();
            p.split('#').last().unwrap_or(p).to_string()
        })
        .collect();

    let user_sanitizers: Vec<TaintSanitizer> = parsed
        .sanitizer
        .iter()
        .map(|s| TaintSanitizer { node_id_pattern: s.pattern.clone() })
        .collect();

    let user_rules = TaintRules {
        sinks: user_sinks,
        sanitizers: user_sanitizers,
        extra_sink_name_patterns: user_extra,
        max_hops: 12,
    };

    base.merge(user_rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_defaults() {
        let tmp = std::env::temp_dir().join("cih-taint-cfg-test-missing");
        std::fs::create_dir_all(&tmp).unwrap();
        let rules = load_taint_rules(&tmp);
        assert!(!rules.sinks.is_empty(), "defaults must have sinks");
        assert!(!rules.extra_sink_name_patterns.is_empty(), "defaults must have name patterns");
    }

    #[test]
    fn custom_sink_merged_with_defaults() {
        let tmp = std::env::temp_dir().join("cih-taint-cfg-test-custom");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("cih.taint.toml"),
            r#"
[[sink]]
pattern = "MyDao#customExecute"
category = "sql"

[[sanitizer]]
pattern = "MyValidator#sanitize"
"#,
        )
        .unwrap();

        let rules = load_taint_rules(&tmp);

        // Custom sink must be present
        assert!(
            rules.sinks.iter().any(|s| s.node_id_pattern == "MyDao#customExecute"),
            "custom sink not found in merged rules"
        );
        // Default sinks must still be present
        assert!(
            rules.sinks.iter().any(|s| s.node_id_pattern == "Runtime#exec"),
            "default sink missing after merge"
        );
        // Custom sanitizer must be present
        assert!(
            rules.sanitizers.iter().any(|s| s.node_id_pattern == "MyValidator#sanitize"),
            "custom sanitizer not found"
        );
        // extra_sink_name_patterns must include the extracted method name
        assert!(
            rules.extra_sink_name_patterns.iter().any(|p| p == "customExecute"),
            "method name not extracted into extra_sink_name_patterns"
        );
    }

    #[test]
    fn extend_defaults_false_replaces_defaults() {
        let tmp = std::env::temp_dir().join("cih-taint-cfg-test-replace");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            tmp.join("cih.taint.toml"),
            r#"
[settings]
extend_defaults = false

[[sink]]
pattern = "OnlySink#run"
"#,
        )
        .unwrap();

        let rules = load_taint_rules(&tmp);
        assert_eq!(rules.sinks.len(), 1, "only the custom sink should be present");
        assert_eq!(rules.sinks[0].node_id_pattern, "OnlySink#run");
    }
}
