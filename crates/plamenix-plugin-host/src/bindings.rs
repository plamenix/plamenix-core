//! WIT-generated bindings for `wit/plamenix.wit`.
//!
//! `wasmtime::component::bindgen!` expands at compile time into:
//!
//! * `PlamenixPlugin` — the world struct, with `instantiate_async` and
//!   typed accessors for the imported and exported interfaces.
//! * `plamenix::plugin::host::Host` — the trait the host implements
//!   to satisfy the plugin's imports.
//! * `plamenix::plugin::plugin::HostPluginInterface` — exported plugin
//!   accessor (`call_activate`).
//!
//! Phase B touches only the import side; Phase C wires the export side.

#![allow(missing_docs, clippy::all)]

wasmtime::component::bindgen!({
    path: "wit",
    world: "plamenix-plugin",
    async: true,
    trappable_imports: true,
});
