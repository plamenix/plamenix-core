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
pub mod concurrency;
pub mod dispatch;
pub mod epoch;
pub mod error;
pub mod event_bus;
pub mod host;
pub mod host_impl;
pub mod instance;
pub mod limits;
pub mod loader;
pub mod manifest;
pub mod plx;
pub mod signing;
pub mod supervisor;

pub use activator::{
    ActivationOutcome, activate, activate_into_registry, activate_with_state,
    register_event_subscriptions, unregister_event_subscriptions,
};
pub use capability::{LogicalDir, OsKeyring, Permission, PermissionGrant, PermissionSet};
pub use concurrency::{CallPermit, InFlightAcquireError, InFlightLimiter, MAX_IN_FLIGHT_DEFAULT};
pub use dispatch::{Delivery, DispatchOutcome, dispatch_event};
pub use epoch::{
    BACKGROUND_DEADLINE_MS, CallClass, EPOCH_TICK_MS, EpochTicker, INTERACTIVE_DEADLINE_MS,
};
pub use error::PluginError;
pub use event_bus::{EventBus, MatchedSubscriber, matches_pattern, tokenise_pattern};
pub use host::PluginHost;
pub use host_impl::{HostState, LogLevel, LogSink, RecordedLog, SessionSlot, register_host};
pub use instance::{InstanceRegistry, PluginInstance};
pub use limits::{
    DEFAULT_MAX_INSTANCES, DEFAULT_MAX_MEMORIES, DEFAULT_MAX_MEMORY_BYTES,
    DEFAULT_MAX_TABLE_ELEMENTS, DEFAULT_MAX_TABLES, ResourceLimits, ResourceLimitsBuilder,
    ResourceLimitsOverride,
};
pub use loader::{StagedPlugin, load};
pub use manifest::{
    Contributions, Edition, EntryPoints, Manifest, PluginMetadata, RestartPolicy, RuntimeFlags,
    RuntimeLimits, SidebarPanel,
};
pub use plx::{
    MANIFEST_NAME, RESOURCES_DIR, UI_NAME, WASM_NAME, extract_plx, extract_plx_bytes,
    peek_plx_manifest, peek_plx_manifest_bytes, write_plx,
};
pub use signing::{
    ALGORITHM, PluginSignature, SIGNATURE_NAME, VerificationOutcome, compute_archive_digest,
    embed_signature_into_archive, sign_archive, sign_archive_bytes, verify_archive,
    verify_archive_bytes,
};
pub use supervisor::{
    CrashBudget, CrashBudgetState, DisableReason, ExitReason, PluginStatus, PluginSupervisionState,
    RestartDecision, Supervisor,
};
