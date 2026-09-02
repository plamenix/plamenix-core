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
    /// Explicit `charset` override, otherwise inherits the profile's
    /// stored charset (falling back to `UTF8` when neither is set).
    pub charset: Option<String>,
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
    // Runtime values take priority over the keyring-stored ones. The
    // connect dialog's password field says "Leave empty to use the
    // password stored in your keyring" — empty / absent runtime →
    // fall back to keyring, anything else → user explicitly overrode.
    let runtime_password = runtime.password.as_deref().filter(|s| !s.is_empty());
    let password = match (runtime_password, &profile.password_keyring_ref) {
        (Some(plain), _) => plain.to_owned(),
        (None, Some(account)) => secrets.retrieve(&SecretRef::new(service, account.clone()))?,
        (None, None) => String::new(),
    };

    let runtime_encryption_key = runtime.encryption_key.as_deref().filter(|s| !s.is_empty());
    let encryption_key = match (runtime_encryption_key, &profile.encryption_key_keyring_ref) {
        (Some(plain), _) => Some(plain.to_owned()),
        (None, Some(account)) => Some(secrets.retrieve(&SecretRef::new(service, account.clone()))?),
        (None, None) => None,
    };

    Ok(ConnectionConfig {
        host: profile.host.clone(),
        port: profile.port,
        database: profile.database.clone(),
        user: profile.user.clone(),
        password,
        encryption_key,
        fbclient_path: overrides
            .fbclient_path
            .clone()
            .or_else(|| profile.fbclient_path.clone()),
        charset: overrides
            .charset
            .clone()
            .or_else(|| profile.charset.clone()),
        encryption_required: overrides
            .encryption_required
            .unwrap_or(profile.encryption_required),
        embedded: profile.embedded,
    })
}
