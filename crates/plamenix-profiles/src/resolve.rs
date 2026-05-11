//! Combine a [`Profile`] with runtime user input and a
//! [`plamenix_secrets::SecretStore`] into a connect-ready
//! [`plamenix_types::ConnectionConfig`].

use plamenix_secrets::{SecretRef, SecretStore};
use plamenix_types::ConnectionConfig;

use crate::error::ProfileError;
use crate::profile::Profile;

/// Free-form values the user supplies at connect time.
///
/// Profiles never persist plaintext secrets. When a profile has no
/// keyring ref, the connect dialog asks the user for the value and
/// passes it here. When a profile **does** have a keyring ref, this
/// struct's matching field is ignored — the secret comes from the
/// keyring instead.
#[derive(Clone, Debug, Default)]
pub struct RuntimeSecrets {
    /// Password typed in by the user (or fetched from a runtime
    /// keyring entry the caller is managing itself).
    pub password: Option<String>,
    /// Encryption key entered by the user, when targeting an
    /// at-rest-encrypted database without a stored ref.
    pub encryption_key: Option<String>,
}

/// Optional knobs the host wants to set on top of the profile.
///
/// Profiles record `pure_rust` and `encryption_required`, but a caller
/// can override on a per-connect basis (e.g. "Pure-Rust" toggle in the
/// connect dialog forces pure-rust without editing the saved profile).
#[derive(Clone, Debug, Default)]
pub struct ConnectOverrides {
    /// Forces a specific `pure_rust` flag, otherwise inherits the
    /// profile's value.
    pub pure_rust: Option<bool>,
    /// Forces a specific `encryption_required` flag, otherwise inherits
    /// the profile's value.
    pub encryption_required: Option<bool>,
    /// Explicit `fbclient_path` override, otherwise resolved via the
    /// usual `PLAMENIX_FBCLIENT_PATH` chain in `plamenix-db`.
    pub fbclient_path: Option<String>,
}

/// Returns the effective `pure_rust` flag after applying overrides.
#[must_use]
pub fn resolve_pure_rust(profile: &Profile, overrides: &ConnectOverrides) -> bool {
    overrides.pure_rust.unwrap_or(profile.pure_rust)
}

/// Returns a [`ConnectionConfig`] ready to hand to `DbDriver::connect`.
///
/// Pulls each secret from `secrets` when the profile has a ref;
/// otherwise reads from `runtime_secrets`. Empty strings are treated
/// as "no value" and stay as `None` on the resulting config.
///
/// # Errors
///
/// Returns [`ProfileError::Secret`] when the secret store refuses a
/// keyring lookup the profile required.
pub fn resolve_connection_config(
    profile: &Profile,
    secrets: &dyn SecretStore,
    service: &str,
    runtime: &RuntimeSecrets,
    overrides: &ConnectOverrides,
) -> Result<ConnectionConfig, ProfileError> {
    let password = match (&profile.password_keyring_ref, &runtime.password) {
        (Some(account), _) => {
            secrets.retrieve(&SecretRef::new(service, account.clone()))?
        }
        (None, Some(plain)) => plain.clone(),
        (None, None) => String::new(),
    };

    let encryption_key = match (&profile.encryption_key_keyring_ref, &runtime.encryption_key) {
        (Some(account), _) => Some(
            secrets.retrieve(&SecretRef::new(service, account.clone()))?,
        ),
        (None, Some(plain)) if !plain.is_empty() => Some(plain.clone()),
        _ => None,
    };

    Ok(ConnectionConfig {
        host: profile.host.clone(),
        port: profile.port,
        database: profile.database.clone(),
        user: profile.user.clone(),
        password,
        encryption_key,
        fbclient_path: overrides.fbclient_path.clone(),
        encryption_required: overrides.encryption_required.unwrap_or(profile.encryption_required),
    })
}
