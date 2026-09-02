//! Plamenix plugin authoring SDK.
//!
//! `plamenix-plugin-sdk` is the public surface external plugin authors
//! compile against. It deliberately stays small: WASM plugins drive
//! wit-bindgen themselves against the bundled WIT contract, and native
//! activation protocol.
//!
//! # WASM plugins
//!
//! WASM plugins target the `plamenix:plugin` WIT package shipped at
//! `wit/plamenix.wit` (a verbatim copy of the host's contract). Copy
//! the file into your plugin crate's `wit/` directory and drive
//! wit-bindgen as usual:
//!
//! ```ignore
//! wit_bindgen::generate!({
//!     path: "wit",
//!     world: "plamenix-plugin",
//! });
//!
//! struct MyPlugin;
//!
//! impl exports::plamenix::plugin::plugin::Guest for MyPlugin {
//!     fn activate() -> exports::plamenix::plugin::plugin::Activation {
//!         exports::plamenix::plugin::plugin::Activation::Ok
//!     }
//! }
//!
//! export!(MyPlugin);
//! ```
//!
//! Target `wasm32-wasip2`; pin `wit-bindgen` to the host's version
//! (currently 0.46) to dodge ABI drift.
//!
//! # What ships here vs in the host
//!
//! * **Here:** the WIT contract a plugin author compiles against.
//! * **In `plamenix-plugin-host`:** the host-side machinery that
//!   loads bundles, parses manifests, and drives wasmtime. Plugin
//!   authors never depend on that crate.

#![forbid(unsafe_code)]

/// The verbatim WIT contract Plamenix plugins target. Embedded as a
/// `&str` for tooling that wants to consume the schema without
/// touching the on-disk file.
pub const WIT_CONTRACT: &str = include_str!("../wit/plamenix.wit");
