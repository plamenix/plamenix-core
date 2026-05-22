//! Plamenix plugin host.
//!
//! `plamenix-plugin-host` is the WASM Component Model runtime that
//! powers Plamenix's plugin system. It owns the wasmtime engine,
//! parses `manifest.toml`, enforces the capability model documented in
//! `../../../docs/capability-model.md`, and stages plugin bundles for
//! activation.
//!
//! # Phases
//!
//! This crate ships in three phases:
//!
//! * **Phase A** — manifest parser, capability grammar, wasmtime
//!   engine wrapper, bundle loader that stops at component compilation.
//! * **Phase B** — wit-bindgen plumbing for the host import (`log`,
//!   `host-version`) and plugin export (`activate`). Plugins
//!   instantiate inside a per-plugin store and their `activate()` is
//!   called.
//! * **Phase C (this revision)** — `examples/hello-plugin/` ships a
//!   real Rust plugin compiled to `wasm32-wasip2`; an integration test
//!   loads it, activates it, and asserts the host received the plugin's
//!   `log` event via an opt-in capture sink on [`HostState`].
//!
//! The crate is conservative on purpose: WASM Component Model is
//! young, and Plamenix needs the M1 install path to be solid before
//! growing the surface.

#![forbid(unsafe_code)]

pub mod activator;
pub mod bindings;
pub mod capability;
pub mod error;
pub mod host;
pub mod host_impl;
pub mod loader;
pub mod manifest;
pub mod subprocess;

pub use activator::{ActivationOutcome, activate, activate_with_state};
pub use capability::{LogicalDir, OsKeyring, Permission, PermissionSet};
pub use error::PluginError;
pub use host::PluginHost;
pub use host_impl::{HostState, LogLevel, LogSink, RecordedLog, register_host};
pub use loader::{StagedPlugin, load};
pub use manifest::{
    Contributions, EntryPoints, Manifest, PluginMetadata, RuntimeFlags, SidebarPanel,
};
pub use subprocess::activate_subprocess;
