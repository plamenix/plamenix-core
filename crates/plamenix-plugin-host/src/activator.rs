//! Plugin activation pipeline.
//!
//! `activate` consumes a [`StagedPlugin`] (produced by
//! [`crate::loader::load`]), instantiates it inside a fresh wasmtime
//! [`Store`], links the host imports via [`crate::host_impl`], and
//! calls the plugin's `activate` export. The returned
//! [`ActivationOutcome`] mirrors the WIT `activation` variant.

use wasmtime::Store;
use wasmtime::component::Linker;

use crate::bindings::PlamenixPlugin;
use crate::bindings::exports::plamenix::plugin::plugin as wit_plugin;
use crate::error::PluginError;
use crate::host::PluginHost;
use crate::host_impl::{HostState, register_host};
use crate::loader::StagedPlugin;

/// Outcome of calling a plugin's `activate` export.
///
/// Mirrors the WIT `activation` variant.
#[derive(Debug, Clone)]
pub enum ActivationOutcome {
    /// Plugin reported successful activation.
    Ok,
    /// Plugin returned an error message from inside its activate hook.
    Failed(String),
}

impl From<wit_plugin::Activation> for ActivationOutcome {
    fn from(value: wit_plugin::Activation) -> Self {
        match value {
            wit_plugin::Activation::Ok => Self::Ok,
            wit_plugin::Activation::Failed(msg) => Self::Failed(msg),
        }
    }
}

/// Instantiates `staged` and calls its `activate` export.
///
/// Plugins without a wasm half (UI-only) skip activation entirely and
/// return `Ok(ActivationOutcome::Ok)`.
///
/// # Errors
///
/// Returns [`PluginError::Runtime`] if the linker rejects a host
/// import, the store fails to instantiate the component, or the
/// `activate` call traps inside the plugin.
pub async fn activate(
    host: &PluginHost,
    host_version: &str,
    staged: &StagedPlugin,
) -> Result<ActivationOutcome, PluginError> {
    let Some(component) = staged.component.as_ref() else {
        return Ok(ActivationOutcome::Ok);
    };

    let mut linker = Linker::<HostState>::new(host.engine());
    register_host(&mut linker)?;

    let state = HostState::new(&staged.manifest.plugin.id, host_version);
    let mut store = Store::new(host.engine(), state);

    let bindings = PlamenixPlugin::instantiate_async(&mut store, component, &linker)
        .await
        .map_err(|err| PluginError::Runtime(err.to_string()))?;

    let activation = bindings
        .plamenix_plugin_plugin()
        .call_activate(&mut store)
        .await
        .map_err(|err| PluginError::Runtime(err.to_string()))?;

    Ok(ActivationOutcome::from(activation))
}
