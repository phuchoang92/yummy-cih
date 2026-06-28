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
            TaintSink { node_id_pattern: s.pattern.clone(), category, language: None }
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
        .map(|s| TaintSanitizer { node_id_pattern: s.pattern.clone(), language: None })
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
#[path = "taint_config_tests.rs"]
mod tests;
