//! The shared `wasmtime::Engine` wrapper.
//!
//! `PluginHost` owns one [`wasmtime::Engine`] for the process. Cloning
//! it is cheap (the engine is internally reference-counted) and safe to
//! share across threads. Each loaded plugin gets its own
//! [`wasmtime::Store`] when it is instantiated (Phase B); the engine
//! itself stays stateless.
//!
//! # Panic discipline (I8.9)
//!
//! Same rule as [`crate::host_impl`]: this module sits at the
//! wasmtime boundary and runs adjacent to plugin code. `unwrap`,
//! `expect`, and `panic!` are denied outside of tests; surface
//! failure modes through [`PluginError`].

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

use std::sync::Arc;

use wasmtime::{Config, Engine};

use crate::error::PluginError;

/// Host-side handle to the wasmtime runtime.
///
/// Cheap to clone — clones share the underlying [`Engine`].
#[derive(Clone)]
pub struct PluginHost {
    engine: Arc<Engine>,
}

impl PluginHost {
    /// Returns a new host with a default async + component-model engine.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if wasmtime fails to initialise
    /// the engine (typically a platform-support issue).
    pub fn new() -> Result<Self, PluginError> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);
        config.wasm_backtrace_details(wasmtime::WasmBacktraceDetails::Enable);
        // I8.7 — epoch interruption enabled. The host spawns an
        // [`crate::epoch::EpochTicker`] that bumps the engine's
        // counter every 10 ms; the activator + dispatchers call
        // `Store::set_epoch_deadline` per call to preempt runaway
        // plugins. Without the ticker, no preemption happens —
        // hosts that omit the ticker get an effectively-unbounded
        // deadline (defensive: better than crashing the host).
        config.epoch_interruption(true);

        let engine = Engine::new(&config).map_err(|err| PluginError::Runtime(err.to_string()))?;
        Ok(Self {
            engine: Arc::new(engine),
        })
    }

    /// Returns a reference to the shared engine for instantiating
    /// components and stores.
    #[must_use]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginHost").finish_non_exhaustive()
    }
}
