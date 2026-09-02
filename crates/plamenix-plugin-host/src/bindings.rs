//! WIT-generated bindings for `wit/plamenix.wit`.
//!
//! `wasmtime::component::bindgen!` expands at compile time into:
//!
//! * `PluginMinimal` — the world struct, with `instantiate_async` and
//!   typed accessors for the imported and exported interfaces.
//! * `plamenix::plugin::host::Host` — the trait the host implements
//!   to satisfy the plugin's imports.
//! * `plamenix::plugin::plugin::HostPluginInterface` — exported plugin
//!   accessor (`call_activate`).
//!
//! ## One binding for five worlds
//!
//! This generates against `plugin-integrated-desktop`, the largest
//! world, and every tier below it is served by the same binding. That
//! is not a shortcut — it follows from how wasmtime resolves a
//! component:
//!
//! * **Imports** are checked against the *component's* import list, not
//!   the world's. `Linker::typecheck` walks `import_types` and looks
//!   each name up; it never iterates the linker's own definitions. So a
//!   linker holding more than a component needs is fine, and a linker
//!   missing something the component imports is what refuses. That
//!   refusal is the capability enforcement — see
//!   [`crate::link::register_for_world`], which registers only the
//!   interfaces a declared world exposes.
//! * **Exports** are all this struct actually needs, and every world
//!   `include`s `plugin-minimal`, so all five export an identical
//!   `plamenix:plugin/plugin`.
//!
//! The alternative — one `bindgen!` per world — would mint five
//! distinct Rust types for the same interface and force
//! [`crate::instance::PluginInstance`] to become generic over the world,
//! which would ripple into the registry, the dispatcher, and the
//! interceptor path for no gain.

#![allow(missing_docs, clippy::all)]

wasmtime::component::bindgen!({
    path: "wit",
    world: "plugin-integrated-desktop",
    async: true,
    trappable_imports: true,
});

/// The generated world struct, under a name that does not imply a tier.
///
/// Instances of every world are represented by this type; see the
/// module docs for why that is sound.
pub use PluginIntegratedDesktop as PluginBindings;
