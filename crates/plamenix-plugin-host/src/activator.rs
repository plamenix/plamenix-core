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

use std::sync::Arc;

use wasmtime::Store;
use wasmtime::component::Linker;

use crate::bindings::PluginMinimal;
use crate::bindings::exports::plamenix::plugin::plugin as wit_plugin;
use crate::error::PluginError;
use crate::event_bus::EventBus;
use crate::host::PluginHost;
use crate::host_impl::{HostState, register_host};
use crate::instance::{InstanceRegistry, PluginInstance};
use crate::loader::StagedPlugin;
use crate::trap::CallFailure;

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
#[tracing::instrument(
    name = "plugin.activate",
    skip(host, staged),
    fields(
        plugin_id = %staged.manifest.plugin.id,
        version = %staged.manifest.plugin.version,
    ),
)]
pub async fn activate(
    host: &PluginHost,
    host_version: &str,
    staged: &StagedPlugin,
) -> Result<ActivationOutcome, PluginError> {
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

    // I8.6 — resolve the per-store ResourceLimits from the
    // manifest's `[runtime.limits]` table on top of the I8.6
    // defaults. The manifest field is optional; missing keys keep the
    // defaults.
    let resolved_limits = staged.manifest.runtime.limits.resolve();
    let state = state.with_resource_limits(resolved_limits);
    let mut store = Store::new(host.engine(), state);
    // Attach the host-state-owned ResourceLimits as the store's
    // limiter. The closure borrows the limits out of HostState;
    // wasmtime consults it on every memory.grow / table.grow attempt
    // + when querying the count caps.
    store.limiter(|state| &mut state.resource_limits);
    // I8.7 — `activate` is treated as a background-class call: it
    // may need to allocate, link, and run setup logic. Subsequent
    // event-dispatch calls (I6.x) use interactive deadlines via
    // their own dispatcher path. The deadline applies until the
    // host explicitly extends it.
    store.set_epoch_deadline(crate::epoch::CallClass::Background.deadline_ticks());

    let bindings = PluginMinimal::instantiate_async(&mut store, component, &linker)
        .await
        .map_err(|err| PluginError::Runtime(CallFailure::from_error(&err).to_string()))?;

    let activation = bindings
        .plamenix_plugin_plugin()
        .call_activate(&mut store)
        .await
        .map_err(|err| PluginError::Runtime(CallFailure::from_error(&err).to_string()))?;

    Ok(ActivationOutcome::from(activation))
}

/// Activate `staged` AND keep the live wasmtime store + bindings
/// alive after `activate` returns by registering them as a
/// [`PluginInstance`] in `registry` (I8.2).
///
/// Compared to [`activate_with_state`], this variant gives the host
/// a handle to dispatch later plugin calls (event handlers, command
/// invocations) into the SAME store the activation ran in — preserving
/// the plugin's in-memory state across calls.
///
/// Behaviour:
///
/// - If the plugin has no wasm component (UI-only), no instance is
///   registered and `Ok(ActivationOutcome::Ok)` is returned.
/// - If `activate` returns `Failed`, the instance is NOT registered —
///   the failed plugin's store drops at the end of this call so
///   nothing in the registry references it. Returning the
///   `ActivationOutcome::Failed` lets the host's supervisor record
///   the crash + decide whether to retry.
/// - If an instance already exists under this plugin id, it is
///   REPLACED. The previous instance's `Arc` returns to the caller via
///   the registry's normal drop-on-overwrite path; callers that need
///   to run the plugin's `deactivate` first MUST call
///   [`InstanceRegistry::remove`] before invoking this function.
///
/// # Errors
///
/// Same as [`activate_with_state`], plus [`PluginError::Runtime`]
/// when the registry's lock is poisoned.
pub async fn activate_into_registry(
    host: &PluginHost,
    state: HostState,
    staged: &StagedPlugin,
    registry: &InstanceRegistry,
) -> Result<ActivationOutcome, PluginError> {
    let Some(component) = staged.component.as_ref() else {
        return Ok(ActivationOutcome::Ok);
    };

    let mut linker = Linker::<HostState>::new(host.engine());
    register_host(&mut linker)?;

    // I8.6 — resolve the per-store ResourceLimits from the
    // manifest's `[runtime.limits]` table on top of the I8.6
    // defaults. The manifest field is optional; missing keys keep the
    // defaults.
    let resolved_limits = staged.manifest.runtime.limits.resolve();
    let state = state.with_resource_limits(resolved_limits);
    let mut store = Store::new(host.engine(), state);
    // Attach the host-state-owned ResourceLimits as the store's
    // limiter. The closure borrows the limits out of HostState;
    // wasmtime consults it on every memory.grow / table.grow attempt
    // + when querying the count caps.
    store.limiter(|state| &mut state.resource_limits);
    // I8.7 — `activate` is treated as a background-class call: it
    // may need to allocate, link, and run setup logic. Subsequent
    // event-dispatch calls (I6.x) use interactive deadlines via
    // their own dispatcher path. The deadline applies until the
    // host explicitly extends it.
    store.set_epoch_deadline(crate::epoch::CallClass::Background.deadline_ticks());

    let bindings = PluginMinimal::instantiate_async(&mut store, component, &linker)
        .await
        .map_err(|err| PluginError::Runtime(CallFailure::from_error(&err).to_string()))?;

    let activation = bindings
        .plamenix_plugin_plugin()
        .call_activate(&mut store)
        .await
        .map_err(|err| PluginError::Runtime(CallFailure::from_error(&err).to_string()))?;

    let outcome = ActivationOutcome::from(activation);
    if matches!(outcome, ActivationOutcome::Ok) {
        // I8.8 — resolve the per-plugin in-flight cap from the
        // manifest's `[runtime.limits].max_in_flight` on top of the
        // default. Missing field keeps the default.
        let in_flight_capacity = staged
            .manifest
            .runtime
            .limits
            .max_in_flight
            .unwrap_or(crate::concurrency::MAX_IN_FLIGHT_DEFAULT);
        let in_flight = crate::concurrency::InFlightLimiter::new(in_flight_capacity);
        let instance = Arc::new(PluginInstance::with_in_flight_limiter(
            staged.manifest.plugin.id.clone(),
            store,
            bindings,
            in_flight,
        ));
        registry.insert(instance)?;
    }
    Ok(outcome)
}

/// Registers every `[[contributions.event_subscriptions]]` entry from
/// `staged.manifest` against the supplied [`EventBus`] (I6.2).
/// Call this once after a successful activation so the plugin starts
/// receiving events. Patterns were validated at manifest parse time,
/// but `EventBus::subscribe` still returns a `Result` for empty
/// plugin id / mutex poison cases — propagated as
/// [`PluginError::Runtime`].
///
/// The reciprocal call, [`unregister_event_subscriptions`], runs at
/// supervisor teardown (or whenever a plugin deactivates).
///
/// # Errors
///
/// Returns [`PluginError::Runtime`] when `EventBus::subscribe`
/// rejects an entry — typically because the plugin id is empty
/// (shouldn't happen post-manifest-validation but defensive) or the
/// bus mutex is poisoned.
pub fn register_event_subscriptions(
    bus: &Arc<EventBus>,
    staged: &StagedPlugin,
) -> Result<usize, PluginError> {
    let plugin_id = &staged.manifest.plugin.id;
    let mut count = 0;
    for pattern in &staged.manifest.contributions.event_subscriptions {
        bus.subscribe(plugin_id.clone(), pattern.clone())
            .map_err(PluginError::Runtime)?;
        count += 1;
    }
    Ok(count)
}

/// Drops every subscription registered under `plugin_id` from the
/// supplied [`EventBus`] (I6.2). Returns the number of subscriptions
/// removed. Idempotent — calling on a plugin id with no
/// subscriptions returns `0`.
///
/// Pair with [`register_event_subscriptions`] in the supervisor's
/// plugin-lifecycle code: subscribe on activate, unsubscribe on
/// deactivate (or supervisor teardown / crash recovery).
pub fn unregister_event_subscriptions(bus: &Arc<EventBus>, plugin_id: &str) -> usize {
    bus.unsubscribe_by_plugin(plugin_id)
}
