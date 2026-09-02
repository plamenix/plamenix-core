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
use specta::Type;
use uuid::Uuid;

/// Stable identifier for a profile across renames.
///
/// Allocated with `ProfileId::new()` on first save and immutable after.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, Type)]
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
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
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
    /// Optional accent-palette id (`blue`, `amber`, `rose`, …) the UI
    /// uses to tint this profile's tabs and status dot. Lets users
    /// visually distinguish dev/staging/prod connections. `None` means
    /// "no tint, use the default theme accent".
    #[serde(default)]
    pub color: Option<String>,
    /// Epoch milliseconds (UTC) when this profile was first saved.
    /// Stable across edits; the store preserves the original value
    /// when overwriting an existing entry. Older profile files that
    /// pre-date this field deserialise with `0`.
    #[serde(default)]
    pub created_at: i64,
    /// Epoch milliseconds (UTC) of the most recent successful connect
    /// using this profile. `None` until the user has connected at
    /// least once. Surfaces in the profile picker as a relative
    /// "Last used Xm ago" hint.
    #[serde(default)]
    pub last_used_at: Option<i64>,
    /// Epoch milliseconds (UTC) of the most recent explicit
    /// disconnect against this profile. Stamped only when the user
    /// (or the app on their behalf) calls Disconnect — not on tab
    /// close or app shutdown. Pairs with `last_used_at` so the
    /// profile picker can render "Used X · Disconnected Y" hints.
    #[serde(default)]
    pub last_disconnected_at: Option<i64>,
    /// Optional absolute path to the Firebird native client library
    /// (`libfbclient.dylib` / `.so` / `fbclient.dll`). When set,
    /// `plamenix-db` hands it to `rsfbclient::with_dyn_load` so the
    /// session attaches via this specific build (useful when multiple
    /// Firebird versions are installed side-by-side). `None` falls
    /// back to the usual `PLAMENIX_FBCLIENT_PATH` env chain. Ignored
    /// entirely when `pure_rust` is `true`.
    #[serde(default)]
    pub fbclient_path: Option<String>,
    /// Wire charset for the session. `None` falls back to `UTF8`.
    /// Accepted values match `rsfbclient_core::Charset::from_str` —
    /// see `plamenix_types::ConnectionConfig::charset` for the list.
    #[serde(default)]
    pub charset: Option<String>,
    /// `true` when the profile attaches via Firebird's embedded engine
    /// (the `database` field is a local file path; `host`/`port` are
    /// ignored). Defaults to `false` so legacy profiles continue to
    /// behave as remote-server connections.
    #[serde(default)]
    pub embedded: bool,
}

impl Profile {
    /// Builds a fresh profile with a new id, the supplied fields, and
    /// `created_at` stamped with the current epoch milliseconds.
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
            color: None,
            created_at: now_epoch_ms(),
            last_used_at: None,
            last_disconnected_at: None,
            fbclient_path: None,
            charset: None,
            embedded: false,
        }
    }
}

/// Wall-clock helper. Used by `Profile::new` and by the host's
/// "profile touched on connect" path; living here keeps the time
/// source in one place.
#[must_use]
pub fn now_epoch_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
