//! Host-side implementation of the WIT-imported `host` interface.
//!
//! The state lives in [`HostState`], which is passed as the data type
//! of every plugin's [`wasmtime::Store`]. Each `Store` is per-plugin
//! and short-lived; `HostState` therefore holds plugin-scoped
//! information (the bundle id, host version, etc.) plus shared, cheap
//! references to host services that plugins may need.

use std::sync::{Arc, Mutex};

use wasmtime::component::{Linker, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView};

use crate::bindings::plamenix::plugin::host as wit_host;
use crate::error::PluginError;

/// Severity level for [`RecordedLog`], copied from the WIT enum so the
/// public API does not leak the wit-bindgen-generated types.
///
/// Variant names match the WIT `log-level` enum and `tracing::Level`
/// vocabulary; no per-variant documentation is added.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl From<wit_host::LogLevel> for LogLevel {
    fn from(value: wit_host::LogLevel) -> Self {
        match value {
            wit_host::LogLevel::Trace => Self::Trace,
            wit_host::LogLevel::Debug => Self::Debug,
            wit_host::LogLevel::Info => Self::Info,
            wit_host::LogLevel::Warn => Self::Warn,
            wit_host::LogLevel::Error => Self::Error,
        }
    }
}

/// One log event a plugin emitted via the imported `host.log` function.
#[derive(Clone, Debug)]
pub struct RecordedLog {
    /// Severity reported by the plugin.
    pub level: LogLevel,
    /// Message body.
    pub message: String,
}

/// Optional in-memory log sink.
///
/// When set on a [`HostState`], every plugin `log` call appends a
/// [`RecordedLog`] to the shared buffer in addition to forwarding the
/// event to `tracing`. Used by tests; not recommended for production
/// hosts (unbounded growth).
pub type LogSink = Arc<Mutex<Vec<RecordedLog>>>;

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
    /// Optional in-memory capture buffer for plugin logs. `None` in
    /// production hosts; tests set this to assert on plugin output.
    pub log_sink: Option<LogSink>,
    /// WASI 0.2 context. Required because Rust's `wasm32-wasip2` target
    /// auto-links the WASI standard library; plugins that never touch
    /// WASI still need the imports satisfied by the linker.
    pub wasi: WasiCtx,
    /// Resource table shared with `wasmtime-wasi`.
    pub table: ResourceTable,
}

impl HostState {
    /// Returns a fresh host state for the named plugin.
    #[must_use]
    pub fn new(plugin_id: impl Into<String>, host_version: impl Into<String>) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            host_version: host_version.into(),
            log_sink: None,
            wasi: WasiCtxBuilder::new().inherit_stdio().build(),
            table: ResourceTable::new(),
        }
    }

    /// Attaches an in-memory capture sink. Returns `self` for chaining.
    #[must_use]
    pub fn with_log_sink(mut self, sink: LogSink) -> Self {
        self.log_sink = Some(sink);
        self
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi
    }

    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

#[async_trait::async_trait]
impl wit_host::Host for HostState {
    async fn log(&mut self, level: wit_host::LogLevel, message: String) -> wasmtime::Result<()> {
        if let Some(sink) = &self.log_sink
            && let Ok(mut buffer) = sink.lock()
        {
            buffer.push(RecordedLog {
                level: LogLevel::from(level),
                message: message.clone(),
            });
        }

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
    wasmtime_wasi::add_to_linker_async(linker)
        .map_err(|err| PluginError::Runtime(err.to_string()))?;
    wit_host::add_to_linker(linker, |state: &mut HostState| state)
        .map_err(|err| PluginError::Runtime(err.to_string()))
}
