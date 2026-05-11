//! Profile-store error types.

use std::path::PathBuf;

use plamenix_secrets::SecretError;
use thiserror::Error;

/// Errors returned by [`crate::ProfileStore`] implementations and
/// [`crate::resolve_connection_config`].
#[derive(Debug, Error)]
pub enum ProfileError {
    /// Underlying I/O failure on the profiles file or its parent dir.
    #[error("io error at {path}: {source}")]
    Io {
        /// Path the failure occurred at.
        path: PathBuf,
        /// Operating system error.
        #[source]
        source: std::io::Error,
    },

    /// `profiles.json` exists but does not parse as the expected schema.
    #[error("invalid profiles file at {path}: {message}")]
    InvalidFile {
        /// Path of the offending file.
        path: PathBuf,
        /// Parse-error detail.
        message: String,
    },

    /// No profile matches the supplied id.
    #[error("profile not found: {0}")]
    NotFound(String),

    /// The store rejected a write (typically a name collision when
    /// `unique_name` semantics matter; not enforced in M1).
    #[error("profile rejected: {0}")]
    Rejected(&'static str),

    /// Secret-store lookup failed while resolving a profile.
    #[error("secret resolution failed: {0}")]
    Secret(#[from] SecretError),
}
