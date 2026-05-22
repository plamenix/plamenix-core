//! Firebird at-rest encryption state.
//!
//! The host reports the encryption state of an attached database
//! through the `MON$DATABASE.MON$CRYPT_STATE` system column. The
//! values follow Firebird 3.0+ conventions:
//!
//! | Value | Meaning |
//! |-------|---------|
//! | `0`   | unencrypted |
//! | `1`   | encrypted |
//! | `2`   | decrypt in progress |
//! | `3`   | encrypt in progress |
//!
//! `CryptState` is the typed mirror used by [`crate::DbDriver`] and the
//! `encryption_required` enforcement path inside `connect`.

use serde::{Deserialize, Serialize};
use specta::Type;

use crate::error::DbError;

/// Encryption state of an attached Firebird database.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum CryptState {
    /// Database file is unencrypted.
    Unencrypted,
    /// Database file is encrypted; the host satisfied the
    /// `KeyHolder` callback during attach.
    Encrypted,
    /// `ALTER DATABASE DECRYPT` is in progress.
    DecryptInProgress,
    /// `ALTER DATABASE ENCRYPT` is in progress.
    EncryptInProgress,
}

impl CryptState {
    /// Parses the raw integer reported by `MON$CRYPT_STATE`.
    ///
    /// # Errors
    ///
    /// Returns [`DbError::Driver`] when the value is outside the
    /// documented `0..=3` range.
    pub fn from_raw(value: i64) -> Result<Self, DbError> {
        match value {
            0 => Ok(Self::Unencrypted),
            1 => Ok(Self::Encrypted),
            2 => Ok(Self::DecryptInProgress),
            3 => Ok(Self::EncryptInProgress),
            other => Err(DbError::Driver(format!(
                "unknown MON$CRYPT_STATE value: {other}"
            ))),
        }
    }

    /// Returns `true` when the database is in the `Encrypted` state.
    /// `DecryptInProgress` and `EncryptInProgress` count as transient
    /// and do not satisfy `encryption_required`.
    #[must_use]
    pub const fn is_encrypted(self) -> bool {
        matches!(self, Self::Encrypted)
    }
}
