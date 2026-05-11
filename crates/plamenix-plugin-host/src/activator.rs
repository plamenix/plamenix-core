//! Plugin activation pipeline.
//!
//! Two entry points:
//!
//! * [`activate`] — convenience wrapper that builds a default
//!   [`HostState`] from the plugin id and host version.
//! * [`activate_with_state`] — full control; callers supply a
//!   pre-built [`HostState`] (useful for tests that attach a log sink
//!   or for hosts that want to plug in shared services later).
//!
//! Both end by calling the plugin's `activate` export and mapping the
//! WIT `activation` variant to [`ActivationOutcome`].

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

/// Instantiates `staged` with a default host state and calls its
/// `activate` export.
///
/// When the plugin's manifest sets `runtime.requires_subprocess`, the
/// call is dispatched to [`crate::subprocess::activate_subprocess`]
/// instead of the WASM pipeline.
///
/// # Errors
///
/// See [`activate_with_state`] and
/// [`crate::subprocess::activate_subprocess`].
pub async fn activate(
    host: &PluginHost,
    host_version: &str,
    staged: &StagedPlugin,
) -> Result<ActivationOutcome, PluginError> {
    if staged.manifest.runtime.requires_subprocess {
        return crate::subprocess::activate_subprocess(host_version, staged).await;
    }
    let state = HostState::new(&staged.manifest.plugin.id, host_version);
    activate_with_state(host, state, staged).await
}

/// Instantiates `staged` and calls its `activate` export using the
/// supplied [`HostState`].
///
/// Plugins without a wasm half (UI-only) skip activation entirely and
/// return `Ok(ActivationOutcome::Ok)`.
///
/// # Errors
///
/// Returns [`PluginError::Runtime`] if the linker rejects a host
/// import, the store fails to instantiate the component, or the
/// `activate` call traps inside the plugin.
pub async fn activate_with_state(
    host: &PluginHost,
    state: HostState,
    staged: &StagedPlugin,
) -> Result<ActivationOutcome, PluginError> {
    let Some(component) = staged.component.as_ref() else {
        return Ok(ActivationOutcome::Ok);
    };

    let mut linker = Linker::<HostState>::new(host.engine());
    register_host(&mut linker)?;

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
