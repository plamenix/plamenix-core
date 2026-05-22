//! [`SecretStore`] trait + implementations.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::SecretError;

/// Identifies one secret in a [`SecretStore`].
///
/// The OS keyring partitions entries by `(service, account)`. Plamenix
/// uses `service = "dev.plamenix.<edition>"` plus an `account` that
/// encodes the secret's purpose (e.g.
/// `profile:<profile-id>:password`, `profile:<profile-id>:encryption-key`).
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SecretRef {
    /// Service namespace, typically the host application's bundle id.
    pub service: String,
    /// Account key within the service namespace.
    pub account: String,
}

impl SecretRef {
    /// Constructs a new [`SecretRef`].
    #[must_use]
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }
}

/// Dyn-compatible secret store interface.
///
/// Implementations include [`KeyringStore`] (OS keyring) and
/// [`InMemoryStore`] (for tests / headless dev environments).
pub trait SecretStore: Send + Sync {
    /// Stores `secret` at `secret_ref`, overwriting any previous value.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::Backend`] when the underlying store
    /// rejects the write, or [`SecretError::Unavailable`] when the
    /// backend is not usable on this host.
    fn store(&self, secret_ref: &SecretRef, secret: &str) -> Result<(), SecretError>;

    /// Retrieves the secret previously written at `secret_ref`.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::NotFound`] when no entry exists,
    /// otherwise the backend's failure mode.
    fn retrieve(&self, secret_ref: &SecretRef) -> Result<String, SecretError>;

    /// Removes the entry at `secret_ref`. Idempotent: returns `Ok(())`
    /// when nothing was stored.
    ///
    /// # Errors
    ///
    /// Returns the backend's failure mode if the platform cannot
    /// satisfy the delete.
    fn delete(&self, secret_ref: &SecretRef) -> Result<(), SecretError>;
}

/// Real OS-keyring-backed [`SecretStore`]. Each `(service, account)`
/// pair maps to a single keyring entry.
#[derive(Clone, Debug, Default)]
pub struct KeyringStore;

impl KeyringStore {
    /// Returns a fresh keyring-backed store.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl SecretStore for KeyringStore {
    fn store(&self, secret_ref: &SecretRef, secret: &str) -> Result<(), SecretError> {
        let entry = keyring::Entry::new(&secret_ref.service, &secret_ref.account)?;
        entry.set_password(secret)?;
        Ok(())
    }

    fn retrieve(&self, secret_ref: &SecretRef) -> Result<String, SecretError> {
        let entry = keyring::Entry::new(&secret_ref.service, &secret_ref.account)?;
        match entry.get_password() {
            Ok(secret) => Ok(secret),
            Err(keyring::Error::NoEntry) => Err(SecretError::NotFound {
                service: secret_ref.service.clone(),
                account: secret_ref.account.clone(),
            }),
            Err(err) => Err(err.into()),
        }
    }

    fn delete(&self, secret_ref: &SecretRef) -> Result<(), SecretError> {
        let entry = keyring::Entry::new(&secret_ref.service, &secret_ref.account)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }
}

/// In-process [`SecretStore`] used in tests and headless CI shells
/// where the OS keyring is either absent or undesirable. Entries
/// vanish when the process exits.
#[derive(Default)]
pub struct InMemoryStore {
    entries: Mutex<HashMap<SecretRef, String>>,
}

impl InMemoryStore {
    /// Returns a fresh empty in-memory store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for InMemoryStore {
    fn store(&self, secret_ref: &SecretRef, secret: &str) -> Result<(), SecretError> {
        let mut entries = self.entries.lock().map_err(poisoned)?;
        entries.insert(secret_ref.clone(), secret.to_owned());
        drop(entries);
        Ok(())
    }

    fn retrieve(&self, secret_ref: &SecretRef) -> Result<String, SecretError> {
        let entries = self.entries.lock().map_err(poisoned)?;
        let value = entries.get(secret_ref).cloned();
        drop(entries);
        value.ok_or_else(|| SecretError::NotFound {
            service: secret_ref.service.clone(),
            account: secret_ref.account.clone(),
        })
    }

    fn delete(&self, secret_ref: &SecretRef) -> Result<(), SecretError> {
        let mut entries = self.entries.lock().map_err(poisoned)?;
        entries.remove(secret_ref);
        drop(entries);
        Ok(())
    }
}

fn poisoned<T>(_err: T) -> SecretError {
    SecretError::Backend("in-memory store mutex poisoned".into())
}

/// Plain-text JSON-on-disk [`SecretStore`]. Intended for dev / debug
/// builds on macOS where the OS keyring's ACL is bound to the binary
/// hash, so every `cargo build` invalidates "Always Allow" and pops a
/// password prompt on every restart. Release builds keep using
/// [`KeyringStore`].
///
/// Storage is intentionally plain JSON — these are local-dev secrets on
/// the developer's own machine, and the file lives in the
/// per-user application config directory with 0o600 permissions on
/// Unix. Do not use in release builds.
#[derive(Debug)]
pub struct JsonSecretStore {
    path: PathBuf,
    entries: Mutex<HashMap<SecretRef, String>>,
}

impl JsonSecretStore {
    /// Opens the JSON store at `path`. Creates the parent directory and
    /// the file if they do not exist; loads any previously-written
    /// entries.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::Backend`] when the parent directory cannot
    /// be created, the file cannot be read, or the file is present but
    /// not valid JSON.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SecretError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                SecretError::Backend(format!(
                    "failed to create secrets directory {}: {err}",
                    parent.display()
                ))
            })?;
        }
        let entries = if path.exists() {
            let bytes = fs::read(&path).map_err(|err| {
                SecretError::Backend(format!(
                    "failed to read secrets file {}: {err}",
                    path.display()
                ))
            })?;
            if bytes.is_empty() {
                HashMap::new()
            } else {
                let list: Vec<(SecretRef, String)> = serde_json::from_slice(&bytes)
                    .map_err(|err| {
                        SecretError::Backend(format!(
                            "failed to parse secrets file {}: {err}",
                            path.display()
                        ))
                    })?;
                list.into_iter().collect()
            }
        } else {
            HashMap::new()
        };
        Ok(Self {
            path,
            entries: Mutex::new(entries),
        })
    }

    fn flush(&self, entries: &HashMap<SecretRef, String>) -> Result<(), SecretError> {
        let list: Vec<(&SecretRef, &String)> = entries.iter().collect();
        let bytes = serde_json::to_vec_pretty(&list).map_err(|err| {
            SecretError::Backend(format!("failed to serialise secrets: {err}"))
        })?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, &bytes).map_err(|err| {
            SecretError::Backend(format!(
                "failed to write secrets temp file {}: {err}",
                tmp.display()
            ))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
        }
        fs::rename(&tmp, &self.path).map_err(|err| {
            SecretError::Backend(format!(
                "failed to rename secrets temp file into place: {err}"
            ))
        })?;
        Ok(())
    }
}

impl SecretStore for JsonSecretStore {
    fn store(&self, secret_ref: &SecretRef, secret: &str) -> Result<(), SecretError> {
        let mut entries = self.entries.lock().map_err(poisoned)?;
        entries.insert(secret_ref.clone(), secret.to_owned());
        self.flush(&entries)?;
        Ok(())
    }

    fn retrieve(&self, secret_ref: &SecretRef) -> Result<String, SecretError> {
        let entries = self.entries.lock().map_err(poisoned)?;
        entries
            .get(secret_ref)
            .cloned()
            .ok_or_else(|| SecretError::NotFound {
                service: secret_ref.service.clone(),
                account: secret_ref.account.clone(),
            })
    }

    fn delete(&self, secret_ref: &SecretRef) -> Result<(), SecretError> {
        let mut entries = self.entries.lock().map_err(poisoned)?;
        if entries.remove(secret_ref).is_some() {
            self.flush(&entries)?;
        }
        Ok(())
    }
}
