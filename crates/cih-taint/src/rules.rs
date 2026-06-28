//! Built-in taint rules for Java/Spring codebases.
//!
//! Sources: HTTP entry-points and event-listener methods (identified via graph edges).
//! Sinks: dynamic SQL execution, OS process exec, and unsafe file writes.
//! Sanitizers: known output-encoding and SQL-parameterization helpers.

/// What kind of dangerous operation a sink performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkCategory {
    /// Dynamic SQL string passed to a DB execution API.
    Sql,
    /// OS process execution (Runtime.exec, ProcessBuilder).
    Exec,
    /// File-system write with a caller-controlled path or content.
    File,
    /// HTML/JS output without encoding (potential XSS).
    Html,
}

impl SinkCategory {
    pub fn label(self) -> &'static str {
        match self {
            SinkCategory::Sql => "sql",
            SinkCategory::Exec => "exec",
            SinkCategory::File => "file",
            SinkCategory::Html => "html",
        }
    }

    pub fn severity(self) -> &'static str {
        match self {
            SinkCategory::Sql | SinkCategory::Exec => "high",
            SinkCategory::File | SinkCategory::Html => "medium",
        }
    }
}

/// Programming language a taint rule applies to.
///
/// `None` on a rule means "any language" (matches all sources).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Java,
    Kotlin,
    Python,
}

/// A sink pattern matched against the target node ID of a `Calls` edge.
pub struct TaintSink {
    /// Substring matched against the callee node ID (e.g. `"Runtime#exec"`).
    pub node_id_pattern: String,
    pub category: SinkCategory,
    /// Language this rule applies to. `None` matches any language.
    pub language: Option<Language>,
}

/// A sanitizer pattern matched against the callee node ID of a `Calls` edge.
/// When a method on the taint path calls a sanitizer, propagation stops on that branch.
pub struct TaintSanitizer {
    /// Substring matched against the callee node ID.
    pub node_id_pattern: String,
    /// Language this rule applies to. `None` matches any language.
    pub language: Option<Language>,
}

/// Complete ruleset used by the taint BFS pass.
pub struct TaintRules {
    /// Sink patterns checked against `Calls` edge targets (Phase 0 BFS).
    pub sinks: Vec<TaintSink>,
    /// Sanitizer patterns checked against `Calls` edge targets.
    pub sanitizers: Vec<TaintSanitizer>,
    /// Method-name substrings for Phase 1/3 intra-proc IR analysis.
    /// These complement `sinks` for the IR-level detection that only has the
    /// call method name, not the full class-qualified node ID.
    pub extra_sink_name_patterns: Vec<String>,
    /// Maximum number of edges to traverse from a source before giving up.
    pub max_hops: usize,
}

impl TaintRules {
    /// Append `other`'s rules into `self`, deduplicating by pattern string.
    pub fn merge(mut self, other: TaintRules) -> TaintRules {
        let existing_sink_pats: std::collections::HashSet<String> =
            self.sinks.iter().map(|s| s.node_id_pattern.clone()).collect();
        for s in other.sinks {
            if !existing_sink_pats.contains(&s.node_id_pattern) {
                self.sinks.push(s);
            }
        }
        let existing_san_pats: std::collections::HashSet<String> =
            self.sanitizers.iter().map(|s| s.node_id_pattern.clone()).collect();
        for s in other.sanitizers {
            if !existing_san_pats.contains(&s.node_id_pattern) {
                self.sanitizers.push(s);
            }
        }
        let existing_extra: std::collections::HashSet<String> =
            self.extra_sink_name_patterns.iter().cloned().collect();
        for p in other.extra_sink_name_patterns {
            if !existing_extra.contains(&p) {
                self.extra_sink_name_patterns.push(p);
            }
        }
        self
    }
}

/// Built-in rules covering the most common Java/Spring vulnerabilities.
///
/// SQL sinks are also detected via the existing `ExecutesQuery` → dynamic `DbQuery`
/// graph edges (no pattern matching needed for those). These patterns cover additional
/// exec/file/HTML cases where the graph may not yet have explicit edges.
macro_rules! sink {
    ($pat:expr, $cat:expr) => {
        TaintSink { node_id_pattern: $pat.into(), category: $cat, language: Some(Language::Java) }
    };
}
macro_rules! san {
    ($pat:expr) => {
        TaintSanitizer { node_id_pattern: $pat.into(), language: Some(Language::Java) }
    };
}

pub fn default_rules() -> TaintRules {
    TaintRules {
        sinks: vec![
            // OS process execution
            sink!("Runtime#exec",            SinkCategory::Exec),
            sink!("ProcessBuilder#command",  SinkCategory::Exec),
            sink!("ProcessBuilder#start",    SinkCategory::Exec),
            // JDBC raw-string execution (parameterized PreparedStatement is NOT a sink)
            sink!("Statement#execute",       SinkCategory::Sql),
            sink!("Statement#executeQuery",  SinkCategory::Sql),
            sink!("Statement#executeUpdate", SinkCategory::Sql),
            sink!("JdbcTemplate#execute",    SinkCategory::Sql),
            sink!("JdbcTemplate#query",      SinkCategory::Sql),
            sink!("JdbcTemplate#update",     SinkCategory::Sql),
            sink!("NamedParameterJdbcTemplate#query",  SinkCategory::Sql),
            sink!("NamedParameterJdbcTemplate#update", SinkCategory::Sql),
            // File-system writes
            sink!("Files#write",                  SinkCategory::File),
            sink!("Files#createFile",             SinkCategory::File),
            sink!("FileOutputStream#<init>",      SinkCategory::File),
            sink!("FileWriter#<init>",            SinkCategory::File),
            // HTML output (XSS)
            sink!("PrintWriter#print",            SinkCategory::Html),
            sink!("HttpServletResponse#getWriter", SinkCategory::Html),
        ],
        sanitizers: vec![
            // HTML/JS output encoding
            san!("HtmlUtils#htmlEscape"),
            san!("StringEscapeUtils#escapeHtml"),
            san!("ESAPI"),
            san!("Encode#forHtml"),
            // SQL parameterization (PreparedStatement prevents injection)
            san!("PreparedStatement#setString"),
            san!("PreparedStatement#setInt"),
            san!("PreparedStatement#set"),
            // Spring validation
            san!("Validator#validate"),
            san!("BindingResult#hasErrors"),
        ],
        // Method-name substrings for Phase 1/3 intra-proc IR sink detection.
        // These cover common dangerous method names when the class context is unavailable.
        extra_sink_name_patterns: vec![
            "execute".into(),
            "executeQuery".into(),
            "executeUpdate".into(),
            "exec".into(),
            "write".into(),
            "delete".into(),
            "update".into(),
            "insert".into(),
        ],
        max_hops: 12,
    }
}
