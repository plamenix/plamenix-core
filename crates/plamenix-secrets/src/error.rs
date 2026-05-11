//! Secret-store error types.

use thiserror::Error;

/// Errors returned by [`crate::SecretStore`] implementations.
#[derive(Debug, Error)]
pub enum SecretError {
    /// No secret stored at the requested [`crate::SecretRef`].
    #[error("secret not found for service `{service}` account `{account}`")]
    NotFound {
        /// Service namespace from the requested ref.
        service: String,
        /// Account key from the requested ref.
        account: String,
    },

    /// The OS keyring (or the configured backend) refused the operation.
    /// Wraps the platform-specific error string so callers do not have
    /// to depend on the `keyring` crate's error type directly.
    #[error("keyring backend error: {0}")]
    Backend(String),

    /// The configured backend cannot satisfy the request on this host —
    /// e.g. headless CI without `dbus`, or a forced in-memory store.
    #[error("secret backend unavailable: {0}")]
    Unavailable(String),
}

impl From<keyring::Error> for SecretError {
    fn from(err: keyring::Error) -> Self {
        match err {
            keyring::Error::NoEntry => Self::NotFound {
                service: String::new(),
                account: String::new(),
            },
            keyring::Error::NoStorageAccess(_) | keyring::Error::PlatformFailure(_) => {
                Self::Unavailable(err.to_string())
            }
            other => Self::Backend(other.to_string()),
        }
    }
}
