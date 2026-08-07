//! Building a linker that holds exactly what a world exposes.
//!
//! This is where the capability model stops being a document and starts
//! being enforced. A plugin's declared world decides which interfaces
//! go into its linker, and wasmtime refuses to instantiate a component
//! that imports anything absent from it. A `plugin-minimal` plugin
//! whose wasm reaches for `plamenix:plugin/db` does not get a runtime
//! error on first use — it never instantiates.
//!
//! That is the Object Capability Model the WIT header claims: the
//! plugin cannot name what it was not given. A per-call permission
//! check, which [`crate::gate`] also does, is a weaker thing — it
//! assumes the function is callable and asks whether this caller may.
//! Both are worth having, and this one comes first.
//!
//! ## Why one linker shape works for five worlds
//!
//! `Linker::typecheck` walks the *component's* import list and looks
//! each name up. It never iterates the linker's definitions, so extra
//! entries are harmless and a missing entry is the refusal. That is why
//! [`crate::bindings`] can generate against the largest world while
//! this function hands out smaller linkers.

use wasmtime::component::Linker;

use crate::bindings::plamenix::plugin::{
    clipboard as wit_clipboard, command as wit_command, db as wit_db, db_write as wit_db_write,
    event_bus as wit_event_bus, fs as wit_fs, host as wit_host, keyring as wit_keyring,
    net as wit_net, notify as wit_notify, settings as wit_settings,
};
use crate::error::PluginError;
use crate::host_impl::HostState;
use crate::world::PluginWorld;

/// The `get` closure every `add_to_linker` wants.
///
/// A plain `fn` item rather than a closure at each call site: it is
/// `Copy + Send + Sync + 'static` already, and writing the same closure
/// eleven times invites one of them to differ.
fn identity(state: &mut HostState) -> &mut HostState {
    state
}

/// Registers WASI plus exactly the interfaces `world` exposes.
///
/// # Errors
///
/// [`PluginError::Runtime`] if wasmtime rejects a registration, which
/// in practice means a duplicate interface name.
pub fn register_for_world(
    linker: &mut Linker<HostState>,
    world: PluginWorld,
) -> Result<(), PluginError> {
    let wrap = |err: wasmtime::Error| PluginError::Runtime(err.to_string());

    // Not a capability. Rust's `wasm32-wasip2` target auto-links the
    // WASI standard library, so a plugin that never mentions WASI still
    // imports it and would fail to instantiate without this. The
    // filesystem and socket tables behind it are empty; `fs` and `net`
    // are the real surfaces and they are gated below.
    wasmtime_wasi::add_to_linker_async(linker).map_err(wrap)?;
    // Tier 0, and every world includes it.
    wit_host::add_to_linker(linker, identity).map_err(wrap)?;

    if world.includes(PluginWorld::DbReader) {
        wit_db::add_to_linker(linker, identity).map_err(wrap)?;
    }
    if world.includes(PluginWorld::DbWriter) {
        wit_db_write::add_to_linker(linker, identity).map_err(wrap)?;
    }
    if world.includes(PluginWorld::Integrated) {
        wit_fs::add_to_linker(linker, identity).map_err(wrap)?;
        wit_net::add_to_linker(linker, identity).map_err(wrap)?;
        wit_event_bus::add_to_linker(linker, identity).map_err(wrap)?;
        wit_settings::add_to_linker(linker, identity).map_err(wrap)?;
        wit_command::add_to_linker(linker, identity).map_err(wrap)?;
        wit_clipboard::add_to_linker(linker, identity).map_err(wrap)?;
    }
    if world.includes(PluginWorld::IntegratedDesktop) {
        wit_notify::add_to_linker(linker, identity).map_err(wrap)?;
        wit_keyring::add_to_linker(linker, identity).map_err(wrap)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::host::PluginHost;

    #[test]
    fn every_world_produces_a_linker() {
        // Mostly a check that no world registers the same interface
        // twice, which wasmtime rejects and which the `includes` ladder
        // would make easy to do.
        let host = PluginHost::new().unwrap();
        for world in PluginWorld::all() {
            let mut linker = Linker::<HostState>::new(host.engine());
            register_for_world(&mut linker, world)
                .unwrap_or_else(|err| panic!("{} failed to link: {err}", world.name()));
        }
    }
}
