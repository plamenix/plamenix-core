//! Host-side implementation of the WIT-imported `host` interface.
//!
//! The state lives in [`HostState`], which is passed as the data type
//! of every plugin's [`wasmtime::Store`]. Each `Store` is per-plugin
//! and short-lived; `HostState` therefore holds plugin-scoped
//! information (the bundle id, host version, etc.) plus shared, cheap
//! references to host services that plugins may need.

use wasmtime::component::Linker;

use crate::bindings::plamenix::plugin::host as wit_host;
use crate::error::PluginError;

/// Per-plugin host state attached to a wasmtime [`wasmtime::Store`].
///
/// Wraps everything a plugin needs from the host runtime. Plugins call
/// into this through the WIT-generated `Host` trait implementation
/// below.
pub struct HostState {
    /// Identifier from the plugin's manifest, used for log enrichment.
    pub plugin_id: String,
    /// The host's own `SemVer` string, returned to plugins via
    /// `host-version()`.
    pub host_version: String,
}

impl HostState {
    /// Returns a fresh host state for the named plugin.
    #[must_use]
    pub fn new(plugin_id: impl Into<String>, host_version: impl Into<String>) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            host_version: host_version.into(),
        }
    }
}

#[async_trait::async_trait]
impl wit_host::Host for HostState {
    async fn log(&mut self, level: wit_host::LogLevel, message: String) -> wasmtime::Result<()> {
        match level {
            wit_host::LogLevel::Trace => {
                tracing::trace!(plugin = %self.plugin_id, "{}", message);
            }
            wit_host::LogLevel::Debug => {
                tracing::debug!(plugin = %self.plugin_id, "{}", message);
            }
            wit_host::LogLevel::Info => {
                tracing::info!(plugin = %self.plugin_id, "{}", message);
            }
            wit_host::LogLevel::Warn => {
                tracing::warn!(plugin = %self.plugin_id, "{}", message);
            }
            wit_host::LogLevel::Error => {
                tracing::error!(plugin = %self.plugin_id, "{}", message);
            }
        }
        Ok(())
    }

    async fn host_version(&mut self) -> wasmtime::Result<String> {
        Ok(self.host_version.clone())
    }
}

/// Registers the host import surface against `linker`.
///
/// Call once per [`Linker`] before instantiating plugins; the linker
/// can then be reused across many plugin instances that share the same
/// state type.
///
/// # Errors
///
/// Returns [`PluginError::Runtime`] if wasmtime rejects the linker
/// registration (typically a name conflict).
pub fn register_host(linker: &mut Linker<HostState>) -> Result<(), PluginError> {
    wit_host::add_to_linker(linker, |state: &mut HostState| state)
        .map_err(|err| PluginError::Runtime(err.to_string()))
}
