//! Error type for core parsing and validation.

use thiserror::Error;

/// The error type for operations in this crate.
#[derive(Debug, Error)]
pub enum Error {
    /// A string was not a valid [`DepotPath`](crate::DepotPath).
    #[error("invalid depot path {path:?}: {reason}")]
    InvalidPath {
        /// The rejected input.
        path: String,
        /// A short description of the rule that was violated.
        reason: &'static str,
    },
    /// A typemap document could not be parsed.
    #[error("invalid typemap: {0}")]
    InvalidTypemap(String),
}

/// A specialized [`Result`](std::result::Result) type for core operations.
///
/// The error type defaults to [`Error`](enum@Error).
pub type Result<T, E = Error> = std::result::Result<T, E>;
