//! Persistent connection profile.
//!
//! `Profile` is the serializable record a user saves so they can
//! reconnect to a Firebird database without re-entering host details
//! every time. Secrets (password, encryption key) are **never** stored
//! inline: the profile carries an optional pointer
//! ([`SecretRef`]-style `account` string under the host edition's
//! keyring service) and the host fetches the actual secret from
//! `plamenix-secrets` at connect time.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifier for a profile across renames.
///
/// Allocated with `ProfileId::new()` on first save and immutable after.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ProfileId(pub Uuid);

impl ProfileId {
    /// Generates a fresh, random profile id.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

/// A saved connection profile.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    /// Stable identifier.
    pub id: ProfileId,
    /// Human-readable label shown in the profile picker.
    pub name: String,
    /// Firebird server hostname / IP.
    pub host: String,
    /// TCP port. Defaults to 3050 in the connect dialog.
    pub port: u16,
    /// Database path or registered alias.
    pub database: String,
    /// Username.
    pub user: String,
    /// Keyring account key (under the edition's keyring service) where
    /// the password is stored. `None` means "ask the user every time".
    pub password_keyring_ref: Option<String>,
    /// Keyring account key for an encryption key, when targeting an
    /// at-rest-encrypted database.
    pub encryption_key_keyring_ref: Option<String>,
    /// When true, the host refuses to connect to a database whose
    /// `MON$CRYPT_STATE` is not `1`.
    pub encryption_required: bool,
    /// When true, the host uses rsfbclient's pure-Rust backend instead
    /// of the bundled native fbclient.
    pub pure_rust: bool,
}

impl Profile {
    /// Builds a fresh profile with a new id and the supplied fields.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        database: impl Into<String>,
        user: impl Into<String>,
    ) -> Self {
        Self {
            id: ProfileId::new(),
            name: name.into(),
            host: host.into(),
            port,
            database: database.into(),
            user: user.into(),
            password_keyring_ref: None,
            encryption_key_keyring_ref: None,
            encryption_required: false,
            pure_rust: false,
        }
    }
}
