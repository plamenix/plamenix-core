//! Host-side implementation of the WIT-imported `host` interface.
//!
//! The state lives in [`HostState`], which is passed as the data type
//! of every plugin's [`wasmtime::Store`]. Each `Store` is per-plugin
//! and short-lived; `HostState` therefore holds plugin-scoped
//! information (the bundle id, host version, etc.) plus shared, cheap
//! references to host services that plugins may need.
//!
//! # Panic discipline (I8.9)
//!
//! Host imports run inside the wasmtime Store on the same OS thread
//! as the plugin call. A panic here propagates as a Rust unwind into
//! wasmtime; depending on the call shape the unwind can manifest as
//! a plugin trap, a poisoned mutex on the host side, or — in the
//! worst case across the napi-rsfbclient FFI boundary — undefined
//! behaviour. Discipline: `unwrap`, `expect`, and `panic!` are
//! linted as compile errors in this module (and the related host
//! infrastructure modules) outside of tests. Surface errors via
//! [`PluginError`] instead. Tests are exempt via `cfg(test)`.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unimplemented,
        clippy::todo,
        clippy::unreachable
    )
)]

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wasmtime::component::{Linker, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiView};

use crate::bindings::plamenix::plugin::host as wit_host;
use crate::capability::PermissionSet;
use crate::error::PluginError;
use crate::services::{HostServices, NoServices};

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
/// Shared slot holding the calling session's identifier for the
/// duration of a single plugin invocation.
///
/// The web edition is multi-tenant: a single plugin instance serves
/// many concurrent connections. Plugin host imports such as
/// `db.current-session()` must therefore reflect the **caller's**
/// session, not a value baked into the plugin at activation time.
///
/// The napi binding holds one `SessionSlot` per active plugin and
/// updates it via `set_call_context(plugin_id, session_id)` before
/// each invocation; the [`HostState`] that lives inside the wasmtime
/// [`wasmtime::Store`] holds an `Arc` clone of the same slot and reads
/// it from inside host imports.
///
/// Concurrency: writes are bounded by the napi binding's call ordering
/// (one set-context per Fastify request handler before invoking the
/// plugin), and the wasmtime [`wasmtime::Store`] is `!Sync` so a
/// plugin function call holds an exclusive borrow throughout. Reads
/// from host imports therefore see a consistent value for the entire
/// call.
pub type SessionSlot = Arc<Mutex<Option<String>>>;

/// Per-plugin state attached to the wasmtime [`wasmtime::Store`].
///
/// Holds everything the host import surface (`host.log`,
/// `host.host-version`, `host.edition`, future db/fs/net imports)
/// reads or writes during a plugin call. One instance per plugin;
/// lives inside the Store and is shared across all invocations of
/// that plugin. The [`SessionSlot`] field carries per-call context
/// for multi-tenant hosts.
pub struct HostState {
    /// Identifier from the plugin's manifest, used for log enrichment.
    pub plugin_id: String,
    /// The host's own `SemVer` string, returned to plugins via
    /// `host-version()`.
    pub host_version: String,
    /// Edition identifier returned to plugins via `edition()` — either
    /// `"desktop"` or `"web"`. Plugins that target both editions branch
    /// on this when they need edition-specific behaviour at runtime.
    pub edition: String,
    /// Optional in-memory capture buffer for plugin logs. `None` in
    /// production hosts; tests set this to assert on plugin output.
    pub log_sink: Option<LogSink>,
    /// Per-call session identifier. `None` outside an active plugin
    /// call (e.g. during `activate()`, which has no originating
    /// session). See [`SessionSlot`] for the multi-tenant model.
    pub session: SessionSlot,
    /// WASI 0.2 context. Required because Rust's `wasm32-wasip2` target
    /// auto-links the WASI standard library; plugins that never touch
    /// WASI still need the imports satisfied by the linker.
    pub wasi: WasiCtx,
    /// Resource table shared with `wasmtime-wasi`.
    pub table: ResourceTable,
    /// Per-plugin wasmtime resource caps (memory / tables / instances).
    /// Default is the 64 MiB baseline from [`crate::limits`].
    pub resource_limits: crate::limits::ResourceLimits,
    /// What the embedding shell provides on this plugin's behalf —
    /// database, keyring, clipboard, and the rest. Defaults to
    /// [`NoServices`], which provides nothing, so a host that has not
    /// been handed services still constructs.
    pub services: Arc<dyn HostServices>,
    /// The capability tier the plugin declared. Read by
    /// [`crate::gate`] as the first of the three checks a host import
    /// makes, and by [`crate::link`] to decide what to register.
    pub world: crate::world::PluginWorld,
    /// What the plugin's manifest asked for. A capability that is not
    /// in here was never declared, and the plugin is outside its own
    /// contract asking for it.
    pub declared: PermissionSet,
    /// What the user has actually approved.
    ///
    /// A snapshot, refreshed once per plugin call by
    /// [`HostState::refresh_grants`] rather than per import: grants
    /// live behind a lock in the shell, and a call that saw a
    /// permission appear halfway through would be harder to reason
    /// about than one that sees a consistent view.
    pub granted: HashSet<String>,
    /// Root of the plugin's own data directory, if the shell gave it
    /// one. Every `fs` path resolves inside this and nowhere else.
    /// `None` means the plugin has no filesystem at all.
    pub data_dir: Option<PathBuf>,
    /// Events the plugin asked to emit during the call in progress.
    ///
    /// Queued rather than delivered, because the host is holding this
    /// plugin's store for the whole call: dispatching inline would
    /// deadlock a plugin that subscribes to its own topic, and no
    /// epoch deadline would break it because no wasm would be running.
    /// The dispatcher takes these once the call returns. See
    /// [`crate::dispatch`].
    pub outbox: Vec<PendingEmit>,
    /// How many emits deep the current cascade is. Bounded by
    /// [`crate::dispatch::MAX_EMIT_DEPTH`] so a plugin that emits what
    /// it subscribes to terminates.
    pub call_depth: u8,
    /// Epoch ticks spent inside host imports during this call.
    ///
    /// Repaid to the deadline when it fires — see the epoch callback in
    /// [`crate::activator`]. Without this a plugin is preempted for the
    /// host's own I/O latency and the supervisor charges it crash
    /// budget for waiting on our database.
    pub host_import_ticks: u64,
}

/// An event a plugin asked to emit, not yet delivered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingEmit {
    /// Topic the plugin published on.
    pub topic: String,
    /// Opaque payload.
    pub payload: String,
}

impl HostState {
    /// Returns a fresh host state for the named plugin. Edition
    /// defaults to `"desktop"`; callers running inside the web edition
    /// must override via [`HostState::with_edition`].
    ///
    /// The session slot is initialised empty. Multi-tenant callers
    /// share an external slot via [`HostState::with_session_slot`] so
    /// per-call session context can be set from outside the
    /// [`wasmtime::Store`] (which owns this state and otherwise hides
    /// it from subsequent borrows).
    #[must_use]
    pub fn new(plugin_id: impl Into<String>, host_version: impl Into<String>) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            host_version: host_version.into(),
            edition: String::from("desktop"),
            log_sink: None,
            session: Arc::new(Mutex::new(None)),
            wasi: WasiCtxBuilder::new().inherit_stdio().build(),
            table: ResourceTable::new(),
            resource_limits: crate::limits::ResourceLimits::default(),
            services: Arc::new(NoServices),
            world: crate::world::PluginWorld::Minimal,
            declared: PermissionSet::default(),
            granted: HashSet::new(),
            data_dir: None,
            outbox: Vec::new(),
            call_depth: 0,
            host_import_ticks: 0,
        }
    }

    /// Records time spent inside a host import, in epoch ticks.
    ///
    /// Rounded up, so a sub-tick call still repays one and a plugin is
    /// never charged for a host call that was merely fast.
    pub fn charge_host_time(&mut self, elapsed: std::time::Duration) {
        let ticks = elapsed
            .as_millis()
            .div_ceil(u128::from(crate::epoch::EPOCH_TICK_MS));
        self.host_import_ticks = self
            .host_import_ticks
            .saturating_add(u64::try_from(ticks).unwrap_or(u64::MAX));
    }

    /// Attaches what the embedding shell provides.
    ///
    /// Without this the plugin gets [`NoServices`] and every import
    /// beyond logging refuses — which is the correct behaviour for a
    /// host that has not wired itself up, and is what the whole crate
    /// did before these interfaces existed.
    #[must_use]
    pub fn with_services(mut self, services: Arc<dyn HostServices>) -> Self {
        self.services = services;
        self
    }

    /// Records the capability tier the plugin declared.
    #[must_use]
    pub const fn with_world(mut self, world: crate::world::PluginWorld) -> Self {
        self.world = world;
        self
    }

    /// Records what the plugin's manifest asked for.
    #[must_use]
    pub fn with_declared_permissions(mut self, declared: PermissionSet) -> Self {
        self.declared = declared;
        self
    }

    /// Points the plugin's `fs` surface at a directory.
    ///
    /// The host resolves every plugin-supplied path inside this root
    /// and refuses anything that escapes. Leaving it unset is how a
    /// plugin ends up with no filesystem rather than the host's.
    #[must_use]
    pub fn with_data_dir(mut self, data_dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(data_dir.into());
        self
    }

    /// Re-reads the user's grants from the shell.
    ///
    /// Called by the dispatcher once before each plugin call. Doing it
    /// per import instead would take a lock inside every host function
    /// and let a permission change take effect halfway through a call.
    pub fn refresh_grants(&mut self) {
        self.granted = self.services.granted_for(&self.plugin_id);
    }

    /// Overrides the per-store wasmtime resource limits (memory cap,
    /// table/instance/memory caps). The default is 64 MiB linear
    /// memory + the other I8.6 baseline caps.
    #[must_use]
    pub fn with_resource_limits(mut self, limits: crate::limits::ResourceLimits) -> Self {
        self.resource_limits = limits;
        self
    }

    /// Overrides the edition string returned to plugins via the
    /// `edition()` host import. Use `"web"` from the web edition's
    /// napi-wrapped host (Section I1).
    #[must_use]
    pub fn with_edition(mut self, edition: impl Into<String>) -> Self {
        self.edition = edition.into();
        self
    }

    /// Shares an externally owned [`SessionSlot`] with this host state.
    ///
    /// The napi binding uses this to surface the calling session into
    /// host imports without re-entering the wasmtime Store: it holds
    /// the same `Arc` and mutates it via `set_call_context` before
    /// each plugin invocation. Desktop hosts (single-session) typically
    /// skip this and accept the default empty slot.
    #[must_use]
    pub fn with_session_slot(mut self, slot: SessionSlot) -> Self {
        self.session = slot;
        self
    }

    /// Convenience read for host imports that need the calling
    /// session. Returns `None` outside an active plugin call.
    #[must_use]
    pub fn current_session(&self) -> Option<String> {
        self.session.lock().ok().and_then(|guard| guard.clone())
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

    async fn edition(&mut self) -> wasmtime::Result<String> {
        Ok(self.edition.clone())
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
