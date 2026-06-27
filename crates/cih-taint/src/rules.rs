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

/// A sink pattern matched against the target node ID of a `Calls` edge.
pub struct TaintSink {
    /// Substring matched against the callee node ID (e.g. `"Runtime#exec"`).
    pub node_id_pattern: &'static str,
    pub category: SinkCategory,
}

/// A sanitizer pattern matched against the callee node ID of a `Calls` edge.
/// When a method on the taint path calls a sanitizer, propagation stops on that branch.
pub struct TaintSanitizer {
    /// Substring matched against the callee node ID.
    pub node_id_pattern: &'static str,
}

/// Complete ruleset used by the taint BFS pass.
pub struct TaintRules {
    /// Sink patterns checked against `Calls` edge targets.
    pub sinks: Vec<TaintSink>,
    /// Sanitizer patterns checked against `Calls` edge targets.
    pub sanitizers: Vec<TaintSanitizer>,
    /// Maximum number of edges to traverse from a source before giving up.
    pub max_hops: usize,
}

/// Built-in rules covering the most common Java/Spring vulnerabilities.
///
/// SQL sinks are also detected via the existing `ExecutesQuery` → dynamic `DbQuery`
/// graph edges (no pattern matching needed for those). These patterns cover additional
/// exec/file/HTML cases where the graph may not yet have explicit edges.
pub fn default_rules() -> TaintRules {
    TaintRules {
        sinks: vec![
            // OS process execution
            TaintSink { node_id_pattern: "Runtime#exec", category: SinkCategory::Exec },
            TaintSink { node_id_pattern: "ProcessBuilder#command", category: SinkCategory::Exec },
            TaintSink { node_id_pattern: "ProcessBuilder#start", category: SinkCategory::Exec },
            // JDBC raw-string execution (parameterized PreparedStatement is NOT a sink)
            TaintSink { node_id_pattern: "Statement#execute", category: SinkCategory::Sql },
            TaintSink { node_id_pattern: "Statement#executeQuery", category: SinkCategory::Sql },
            TaintSink { node_id_pattern: "Statement#executeUpdate", category: SinkCategory::Sql },
            TaintSink { node_id_pattern: "JdbcTemplate#execute", category: SinkCategory::Sql },
            TaintSink { node_id_pattern: "JdbcTemplate#query", category: SinkCategory::Sql },
            TaintSink { node_id_pattern: "JdbcTemplate#update", category: SinkCategory::Sql },
            TaintSink { node_id_pattern: "NamedParameterJdbcTemplate#query", category: SinkCategory::Sql },
            TaintSink { node_id_pattern: "NamedParameterJdbcTemplate#update", category: SinkCategory::Sql },
            // File-system writes
            TaintSink { node_id_pattern: "Files#write", category: SinkCategory::File },
            TaintSink { node_id_pattern: "Files#createFile", category: SinkCategory::File },
            TaintSink { node_id_pattern: "FileOutputStream#<init>", category: SinkCategory::File },
            TaintSink { node_id_pattern: "FileWriter#<init>", category: SinkCategory::File },
            // HTML output (XSS)
            TaintSink { node_id_pattern: "PrintWriter#print", category: SinkCategory::Html },
            TaintSink { node_id_pattern: "HttpServletResponse#getWriter", category: SinkCategory::Html },
        ],
        sanitizers: vec![
            // HTML/JS output encoding
            TaintSanitizer { node_id_pattern: "HtmlUtils#htmlEscape" },
            TaintSanitizer { node_id_pattern: "StringEscapeUtils#escapeHtml" },
            TaintSanitizer { node_id_pattern: "ESAPI" },
            TaintSanitizer { node_id_pattern: "Encode#forHtml" },
            // SQL parameterization (PreparedStatement prevents injection)
            TaintSanitizer { node_id_pattern: "PreparedStatement#setString" },
            TaintSanitizer { node_id_pattern: "PreparedStatement#setInt" },
            TaintSanitizer { node_id_pattern: "PreparedStatement#set" },
            // Spring validation
            TaintSanitizer { node_id_pattern: "Validator#validate" },
            TaintSanitizer { node_id_pattern: "BindingResult#hasErrors" },
        ],
        max_hops: 12,
    }
}
