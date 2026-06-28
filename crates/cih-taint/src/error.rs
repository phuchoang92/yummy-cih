//! Unified error type for cih-taint operations.

/// Errors produced at the facade boundary of cih-taint.
///
/// Internal helpers (CFG builder, tree-sitter parse, PDG) return `Option<T>` and fail
/// silently — IR unavailability is expected for methods without source. This type is
/// only raised when a caller has explicitly requested an analysis and it cannot proceed.
#[derive(Debug, thiserror::Error)]
pub enum TaintError {
    /// tree-sitter failed to parse or locate a method in a Java source file.
    #[error("Java parse failed for {method_id}: {reason}")]
    ParseFailed { method_id: String, reason: String },

    /// The method node ID did not match the expected `Method:<fqn>#<name>/<arity>` format.
    #[error("invalid method node ID: {0}")]
    InvalidMethodId(String),

    /// An I/O error occurred reading a graph artifact or source file.
    #[error("graph artifact error: {0}")]
    ArtifactIo(#[from] std::io::Error),

    /// Phase 3 was requested but a prerequisite (CFG or reaching-defs) could not be built.
    #[error("missing prerequisite for Phase 3 on {method_id}")]
    MissingPrerequisite { method_id: String },
}

/// Convenience alias for `Result<T, TaintError>`.
pub type TaintResult<T> = Result<T, TaintError>;
