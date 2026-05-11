//! Driver error types.

use thiserror::Error;

/// Errors returned by every [`crate::DbDriver`] method.
///
/// Variants split by *where* the failure originated so callers can decide
/// whether to retry (`Connect`), surface a user-facing diagnostic
/// (`Query`), restart the worker (`Driver`), or treat the call as
/// cancelled (`Cancelled`).
#[derive(Debug, Error)]
pub enum DbError {
    /// Failure during `attach` — typically network unreachable, bad
    /// credentials, missing database, or encryption requirements not met.
    #[error("connect failed: {0}")]
    Connect(String),

    /// Failure during query execution — typically a SQL error reported by
    /// the Firebird engine.
    #[error("query failed: {0}")]
    Query(String),

    /// Failure inside the driver layer itself — usually a panic on the
    /// blocking worker thread or a misuse of the driver API.
    #[error("driver error: {0}")]
    Driver(String),

    /// The session was cancelled before the operation completed (tab
    /// closed, shutdown signal, explicit `cancel()` call).
    #[error("operation cancelled")]
    Cancelled,
}

impl From<rsfbclient::FbError> for DbError {
    fn from(err: rsfbclient::FbError) -> Self {
        Self::Query(err.to_string())
    }
}

impl From<tokio::task::JoinError> for DbError {
    fn from(err: tokio::task::JoinError) -> Self {
        if err.is_cancelled() {
            Self::Cancelled
        } else {
            Self::Driver(err.to_string())
        }
    }
}
