//! Error types for the Leiden algorithm.

/// Errors that can occur during Leiden algorithm execution.
#[derive(Debug, thiserror::Error)]
pub enum LeidenError {
    /// An edge weight is not finite or is negative.
    #[error("invalid edge weight: {weight} (must be finite and non-negative)")]
    InvalidEdgeWeight {
        /// The invalid weight value.
        weight: f64,
    },
    /// CSR structure components have inconsistent lengths.
    #[error("inconsistent CSR structure: {message}")]
    InconsistentStructure {
        /// Description of the inconsistency.
        message: String,
    },
    /// An algorithm parameter is invalid.
    #[error("invalid parameter: {message}")]
    InvalidParameter {
        /// Description of the invalid parameter.
        message: String,
    },
    /// The initial partition does not match the graph.
    #[error("invalid partition: {message}")]
    InvalidPartition {
        /// Description of the mismatch.
        message: String,
    },
}

/// A specialized `Result` type for Leiden algorithm operations.
pub type Result<T> = std::result::Result<T, LeidenError>;
