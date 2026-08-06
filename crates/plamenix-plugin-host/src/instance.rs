//! Per-plugin live wasmtime instance + registry (I8.2).
//!
//! Background — until I8.2, the activator created a fresh
//! [`wasmtime::Store`] each time it activated a plugin and then
//! dropped it at the end of `activate_with_state`. That gave us
//! single-shot activation but no place to dispatch event handlers,
//! command invocations, or any future plugin call once the initial
//! activate returned. The plugin's in-memory state (anything held in
//! the WASM linear memory across calls) would vanish after the
//! activate borrow ended.
//!
//! [`PluginInstance`] is the long-lived owner of a single plugin's
//! `Store<HostState>` + typed bindings. Calls into the plugin reborrow
//! through `&mut Store` via an async-aware [`tokio::sync::Mutex`] so
//! concurrent host code serialises against itself — wasmtime's
//! async-store API requires `&mut Store` per call and is not
//! `Sync` on its own.
//!
//! [`InstanceRegistry`] is the process-wide map. The activator
//! constructs a [`PluginInstance`], hands it to the registry under
//! the plugin's id, and downstream code (event dispatch, command
//! routing) looks the instance up by id. Removing an entry from the
//! registry drops the underlying [`wasmtime::Store`], which in turn
//! releases the plugin's linear memory + tables — the M1 deactivation
//! contract.
//!
//! ## Concurrency model
//!
//! - The registry uses [`std::sync::RwLock`] over its `HashMap`. Most
//!   accesses are reads (lookup); only register/unregister take the
//!   write lock. The registry's mutex is poison-tolerant: a poisoned
//!   lock surfaces as [`PluginError::Runtime`] rather than panicking.
//! - Each [`PluginInstance`] holds its `Store<HostState>` behind a
//!   [`tokio::sync::Mutex`] so plugin-call call sites can `.await`
//!   the lock without blocking the executor.
//!
//! ## Lifetime guarantee
//!
//! When the registry is dropped (host shutdown), every remaining
//! [`PluginInstance`] is dropped with it. Each instance's `Store`
//! drops on the same chain, releasing its linear memory + tables.
//! No state survives the registry — the cross-restart isolation
//! property the supervisor + I8.6 resource limits rely on.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::Mutex;
use wasmtime::Store;

use crate::bindings::PluginMinimal;
use crate::concurrency::{CallPermit, InFlightAcquireError, InFlightLimiter};
use crate::error::PluginError;
use crate::host_impl::HostState;

/// A live wasmtime instance for one plugin.
///
/// Owned by [`InstanceRegistry`]. Once removed (or the registry is
/// dropped), the [`wasmtime::Store`] inside is released and the
/// plugin's linear memory + tables become unreachable.
pub struct PluginInstance {
    /// Manifest plugin id this instance represents.
    plugin_id: String,
    /// The plugin's exclusive wasmtime store. Behind a Tokio mutex
    /// because every plugin call needs `&mut Store` and wasmtime's
    /// async API isn't `Sync`.
    store: Mutex<Store<HostState>>,
    /// Typed bindings for the plugin's exported interface.
    bindings: PluginMinimal,
    /// I8.8 — concurrent in-flight call cap. Dispatchers
    /// `acquire().await` a permit before invoking the plugin; the
    /// permit drops at the end of the call.
    in_flight: InFlightLimiter,
}

impl PluginInstance {
    /// Wraps a freshly instantiated `Store + bindings` pair, with a
    /// default-capacity in-flight limiter (4 concurrent calls).
    ///
    /// Typically constructed by the activator immediately after
    /// `PluginMinimal::instantiate_async` returns. For overrides
    /// (manifest-driven), use [`PluginInstance::with_in_flight_limiter`].
    #[must_use]
    pub fn new(plugin_id: impl Into<String>, store: Store<HostState>, bindings: PluginMinimal) -> Self {
        Self::with_in_flight_limiter(
            plugin_id,
            store,
            bindings,
            InFlightLimiter::with_default_capacity(),
        )
    }

    /// Constructs an instance with an explicit in-flight limiter.
    /// Used by the activator when the manifest's `[runtime.limits]
    /// max_in_flight` overrides the default.
    #[must_use]
    pub fn with_in_flight_limiter(
        plugin_id: impl Into<String>,
        store: Store<HostState>,
        bindings: PluginMinimal,
        in_flight: InFlightLimiter,
    ) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            store: Mutex::new(store),
            bindings,
            in_flight,
        }
    }

    /// Plugin id this instance represents.
    #[must_use]
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Typed bindings; non-mutable so callers can fetch their typed
    /// accessor without grabbing the store lock.
    #[must_use]
    pub fn bindings(&self) -> &PluginMinimal {
        &self.bindings
    }

    /// Locks the store for an exclusive plugin call.
    ///
    /// Returns a guard that derefs to `Store<HostState>`. Hold the
    /// guard across exactly one plugin call site — wasmtime's async
    /// store API requires `&mut Store` per call and rejects nested
    /// re-entry.
    pub async fn lock_store(&self) -> tokio::sync::MutexGuard<'_, Store<HostState>> {
        self.store.lock().await
    }

    /// Access to the in-flight limiter. Dispatchers typically call
    /// [`PluginInstance::acquire_call_permit`] instead; the bare
    /// limiter is exposed for diagnostic queries (capacity,
    /// in_flight, available_permits) used by the Permissions panel.
    #[must_use]
    pub fn in_flight_limiter(&self) -> &InFlightLimiter {
        &self.in_flight
    }

    /// Acquires an in-flight call permit. Hold the permit across
    /// exactly one plugin call site; dropping it returns the slot.
    ///
    /// # Errors
    ///
    /// Returns [`InFlightAcquireError::Closed`] when the limiter has
    /// been closed (plugin being uninstalled).
    pub async fn acquire_call_permit(&self) -> Result<CallPermit, InFlightAcquireError> {
        self.in_flight.acquire().await
    }
}

impl std::fmt::Debug for PluginInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginInstance")
            .field("plugin_id", &self.plugin_id)
            .finish_non_exhaustive()
    }
}

/// Process-wide registry of live plugin instances.
///
/// Cheap to clone (the inner map sits behind an `Arc`); pass clones
/// to the activator, event dispatcher, command router, and the
/// host's deactivation path.
#[derive(Clone, Debug, Default)]
pub struct InstanceRegistry {
    inner: Arc<RwLock<HashMap<String, Arc<PluginInstance>>>>,
}

impl InstanceRegistry {
    /// Returns an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts an instance under its plugin id.
    ///
    /// If an instance already exists under the same id, it is
    /// replaced and the previous instance is dropped (its `Store`
    /// released). The activator MUST first call
    /// [`InstanceRegistry::remove`] on the previous generation if it
    /// needs to run the plugin's `deactivate` before the new instance
    /// takes over.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned.
    pub fn insert(&self, instance: Arc<PluginInstance>) -> Result<(), PluginError> {
        let mut guard = self.inner.write().map_err(lock_poisoned)?;
        guard.insert(instance.plugin_id().to_string(), instance);
        Ok(())
    }

    /// Removes the instance for `plugin_id`. Returns the removed
    /// `Arc<PluginInstance>` so the caller can run cleanup (e.g.
    /// the plugin's `deactivate` export, log a shutdown line)
    /// before the final reference goes out of scope and the Store
    /// drops.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned.
    pub fn remove(&self, plugin_id: &str) -> Result<Option<Arc<PluginInstance>>, PluginError> {
        let mut guard = self.inner.write().map_err(lock_poisoned)?;
        Ok(guard.remove(plugin_id))
    }

    /// Looks up the instance for `plugin_id`, cloning the `Arc` so
    /// the caller can drop the read lock immediately.
    ///
    /// Returns `None` if no instance is registered.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned.
    pub fn get(&self, plugin_id: &str) -> Result<Option<Arc<PluginInstance>>, PluginError> {
        let guard = self.inner.read().map_err(lock_poisoned)?;
        Ok(guard.get(plugin_id).cloned())
    }

    /// Returns true when an instance is currently registered for
    /// `plugin_id`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned.
    pub fn contains(&self, plugin_id: &str) -> Result<bool, PluginError> {
        let guard = self.inner.read().map_err(lock_poisoned)?;
        Ok(guard.contains_key(plugin_id))
    }

    /// Number of live instances.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned.
    pub fn len(&self) -> Result<usize, PluginError> {
        let guard = self.inner.read().map_err(lock_poisoned)?;
        Ok(guard.len())
    }

    /// Returns the sorted plugin ids currently registered. Useful
    /// for the Permissions panel + diagnostic dumps.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned.
    pub fn ids(&self) -> Result<Vec<String>, PluginError> {
        let guard = self.inner.read().map_err(lock_poisoned)?;
        let mut out: Vec<String> = guard.keys().cloned().collect();
        out.sort();
        Ok(out)
    }

    /// Clears every entry. The dropped `Arc`s release each store
    /// transitively. Used by host shutdown + tests.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned.
    pub fn clear(&self) -> Result<(), PluginError> {
        let mut guard = self.inner.write().map_err(lock_poisoned)?;
        guard.clear();
        Ok(())
    }
}

fn lock_poisoned<T>(_err: T) -> PluginError {
    PluginError::Runtime("plugin instance registry lock poisoned".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // PluginMinimal is bindgen-generated and only constructible
    // via `instantiate_async` against a real component. Unit tests
    // here exercise the parts of the registry that don't need a
    // populated entry; insert/remove/get/contains over a populated
    // map are covered by `tests/instance_registry.rs` (integration)
    // which loads the existing `hello-plugin` fixture.

    #[test]
    fn registry_starts_empty() {
        let reg = InstanceRegistry::new();
        assert_eq!(reg.len().unwrap(), 0);
        assert!(reg.ids().unwrap().is_empty());
        assert!(!reg.contains("anything").unwrap());
    }

    #[test]
    fn clone_shares_inner_state() {
        let a = InstanceRegistry::new();
        let b = a.clone();
        assert_eq!(a.len().unwrap(), 0);
        assert_eq!(b.len().unwrap(), 0);
    }

    #[test]
    fn remove_missing_returns_none() {
        let reg = InstanceRegistry::new();
        assert!(reg.remove("nope").unwrap().is_none());
    }

    #[test]
    fn get_missing_returns_none() {
        let reg = InstanceRegistry::new();
        assert!(reg.get("nope").unwrap().is_none());
    }

    #[test]
    fn clear_on_empty_is_noop() {
        let reg = InstanceRegistry::new();
        reg.clear().unwrap();
        assert_eq!(reg.len().unwrap(), 0);
    }
}
