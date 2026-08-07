//! `settings` — a small key/value store scoped to one plugin.
//!
//! Kept in this crate rather than behind
//! [`crate::services::HostServices`] because it needs nothing the shell
//! has: the plugin already has a data directory, and a JSON file in it
//! is the whole implementation. Delegating would mean two shells each
//! writing their own, differing in what they cap and when they flush.
//!
//! Held in memory and written through. A `get` on every keystroke doing
//! a blocking file read on a tokio worker would be the wrong shape, and
//! the whole map is small by construction — see the caps below.
//!
//! ## Scope
//!
//! Per plugin, not per user. On the web edition that means two people
//! using the same server share a plugin's settings. That is a real
//! limitation and it is stated rather than hidden; a plugin storing
//! anything user-specific should key it by session itself.

use std::collections::BTreeMap;

use crate::bindings::plamenix::plugin::settings::{Host as SettingsHost, SettingsError};
use crate::capability::Permission;
use crate::gate::{self, Guard};
use crate::host_impl::HostState;

const READ_GUARD: Guard = Guard::Any(&[Permission::SettingsRead]);
const WRITE_GUARD: Guard = Guard::Any(&[Permission::SettingsWrite]);

/// Longest key accepted.
pub const MAX_KEY_BYTES: usize = 256;
/// Longest value accepted.
pub const MAX_VALUE_BYTES: usize = 16 * 1024;
/// Ceiling on the whole map, so settings cannot become bulk storage.
/// A plugin that needs more has `fs`.
pub const MAX_TOTAL_BYTES: usize = 64 * 1024;

/// File the map lives in, inside the plugin's data directory.
pub const SETTINGS_FILENAME: &str = "settings.json";

/// Reads a plugin's settings from disk.
///
/// Called once at activation. A missing or unreadable file yields an
/// empty map rather than failing the activation: a plugin whose
/// settings were corrupted should start with none, not refuse to run.
#[must_use]
pub fn load(data_dir: Option<&std::path::Path>) -> BTreeMap<String, String> {
    let Some(dir) = data_dir else {
        return BTreeMap::new();
    };
    let Ok(text) = std::fs::read_to_string(dir.join(SETTINGS_FILENAME)) else {
        return BTreeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn flush(state: &HostState) -> Result<(), SettingsError> {
    let Some(dir) = state.data_dir.as_ref() else {
        return Err(SettingsError::Backend(
            "this plugin has no data directory to store settings in".to_owned(),
        ));
    };
    let text = serde_json::to_string_pretty(&state.settings)
        .map_err(|err| SettingsError::Backend(err.to_string()))?;
    std::fs::create_dir_all(dir).map_err(|err| SettingsError::Backend(err.to_string()))?;
    std::fs::write(dir.join(SETTINGS_FILENAME), text)
        .map_err(|err| SettingsError::Backend(err.to_string()))
}

#[async_trait::async_trait]
impl SettingsHost for HostState {
    async fn get(
        &mut self,
        key: String,
    ) -> wasmtime::Result<Result<Option<String>, SettingsError>> {
        if let Err(denial) = gate::check(self, &READ_GUARD) {
            return Ok(Err(SettingsError::CapabilityDenied(denial.to_string())));
        }
        Ok(Ok(self.settings.get(&key).cloned()))
    }

    async fn set(
        &mut self,
        key: String,
        value: String,
    ) -> wasmtime::Result<Result<(), SettingsError>> {
        if let Err(denial) = gate::check(self, &WRITE_GUARD) {
            return Ok(Err(SettingsError::CapabilityDenied(denial.to_string())));
        }
        if key.len() > MAX_KEY_BYTES {
            return Ok(Err(SettingsError::TooLarge(MAX_KEY_BYTES as u32)));
        }
        if value.len() > MAX_VALUE_BYTES {
            return Ok(Err(SettingsError::TooLarge(MAX_VALUE_BYTES as u32)));
        }

        // Measured against what the map would become, not what it is,
        // so the ceiling cannot be stepped over one write at a time.
        let previous = self.settings.get(&key).map_or(0, String::len);
        let projected = total_bytes(&self.settings) + key.len() + value.len() - previous;
        if projected > MAX_TOTAL_BYTES {
            return Ok(Err(SettingsError::TooLarge(MAX_TOTAL_BYTES as u32)));
        }

        let displaced = self.settings.insert(key.clone(), value);
        match flush(self) {
            Ok(()) => Ok(Ok(())),
            Err(err) => {
                // Put the map back. Reporting a failed write while
                // keeping the new value in memory would have the plugin
                // read back something that is not on disk.
                match displaced {
                    Some(old) => self.settings.insert(key, old),
                    None => self.settings.remove(&key),
                };
                Ok(Err(err))
            }
        }
    }

    async fn remove(&mut self, key: String) -> wasmtime::Result<Result<(), SettingsError>> {
        if let Err(denial) = gate::check(self, &WRITE_GUARD) {
            return Ok(Err(SettingsError::CapabilityDenied(denial.to_string())));
        }
        let Some(displaced) = self.settings.remove(&key) else {
            // Removing what is not there is not a failure.
            return Ok(Ok(()));
        };
        match flush(self) {
            Ok(()) => Ok(Ok(())),
            Err(err) => {
                self.settings.insert(key, displaced);
                Ok(Err(err))
            }
        }
    }
}

fn total_bytes(map: &BTreeMap<String, String>) -> usize {
    map.iter().map(|(k, v)| k.len() + v.len()).sum()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;
    use std::sync::Arc;

    use super::*;
    use crate::capability::{PermissionGrant, PermissionSet};
    use crate::services::HostServices;
    use crate::world::PluginWorld;

    struct Granted;
    #[async_trait::async_trait]
    impl HostServices for Granted {
        fn granted_for(&self, _: &str) -> HashSet<String> {
            HashSet::from(["settings.read".to_owned(), "settings.write".to_owned()])
        }
    }

    fn state(dir: &std::path::Path) -> HostState {
        let mut host = HostState::new("dev.plamenix.test", "1.0.0-beta")
            .with_world(PluginWorld::Integrated)
            .with_declared_permissions(PermissionSet {
                required: [Permission::SettingsRead, Permission::SettingsWrite]
                    .into_iter()
                    .map(PermissionGrant::new)
                    .collect(),
                optional: Vec::new(),
            })
            .with_services(Arc::new(Granted))
            .with_data_dir(dir);
        host.refresh_grants();
        host
    }

    #[tokio::test]
    async fn a_value_written_survives_a_reload() {
        // The point of write-through: the in-memory map is a cache, and
        // a plugin restarting must see what it stored.
        let dir = tempfile::tempdir().unwrap();
        let mut host = state(dir.path());
        host.set("theme".to_owned(), "dark".to_owned())
            .await
            .unwrap()
            .unwrap();

        let reloaded = load(Some(dir.path()));
        assert_eq!(reloaded.get("theme").map(String::as_str), Some("dark"));
    }

    #[tokio::test]
    async fn removing_a_key_that_was_never_set_is_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = state(dir.path());
        assert!(host.remove("absent".to_owned()).await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn an_oversized_value_is_refused_with_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = state(dir.path());
        let refused = host
            .set("k".to_owned(), "x".repeat(MAX_VALUE_BYTES + 1))
            .await
            .unwrap();
        match refused {
            Err(SettingsError::TooLarge(limit)) => assert_eq!(limit, MAX_VALUE_BYTES as u32),
            other => panic!("expected the limit to be named, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_total_cannot_be_stepped_over_one_key_at_a_time() {
        // Per-value caps alone would let a plugin write many
        // just-under-the-limit values and use settings as bulk storage.
        let dir = tempfile::tempdir().unwrap();
        let mut host = state(dir.path());
        let chunk = "x".repeat(MAX_VALUE_BYTES);

        let mut refused_at = None;
        for index in 0..16 {
            let outcome = host
                .set(format!("key{index}"), chunk.clone())
                .await
                .unwrap();
            if outcome.is_err() {
                refused_at = Some(index);
                break;
            }
        }

        assert!(
            refused_at.is_some(),
            "the aggregate ceiling never fired; settings would be unbounded storage",
        );
    }

    #[tokio::test]
    async fn overwriting_a_key_does_not_count_the_old_value_twice() {
        // The projection subtracts what is being displaced. Without
        // that, rewriting one large key repeatedly would eventually be
        // refused for no reason.
        let dir = tempfile::tempdir().unwrap();
        let mut host = state(dir.path());
        let chunk = "x".repeat(MAX_VALUE_BYTES);
        for _ in 0..8 {
            host.set("same".to_owned(), chunk.clone())
                .await
                .unwrap()
                .expect("rewriting one key must not accumulate");
        }
    }

    #[tokio::test]
    async fn a_plugin_without_the_capability_is_refused() {
        struct NoGrants;
        #[async_trait::async_trait]
        impl HostServices for NoGrants {
            fn granted_for(&self, _: &str) -> HashSet<String> {
                HashSet::new()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut host = state(dir.path()).with_services(Arc::new(NoGrants));
        host.refresh_grants();

        let refused = host.set("k".to_owned(), "v".to_owned()).await.unwrap();
        assert!(matches!(refused, Err(SettingsError::CapabilityDenied(_))));
    }
}
