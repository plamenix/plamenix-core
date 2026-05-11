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
//! * **Phase A (this revision)** — manifest parser, capability grammar,
//!   wasmtime engine wrapper, bundle loader that stops at component
//!   compilation. No plugin code runs.
//! * **Phase B** — wit-bindgen plumbing for the host import (`log`,
//!   `host-version`) and plugin export (`activate`). Plugins are
//!   instantiated and their `activate()` is called.
//! * **Phase C** — sample `hello-plugin` example, integration tests,
//!   subprocess fallback for `runtime.subprocess` plugins.
//!
//! The crate is conservative on purpose: WASM Component Model is
//! young, and Plamenix needs the M1 install path to be solid before
//! growing the surface.

#![forbid(unsafe_code)]

pub mod capability;
pub mod error;
pub mod host;
pub mod loader;
pub mod manifest;

pub use capability::{LogicalDir, OsKeyring, Permission, PermissionSet};
pub use error::PluginError;
pub use host::PluginHost;
pub use loader::{StagedPlugin, load};
pub use manifest::{EntryPoints, Manifest, PluginMetadata, RuntimeFlags};
