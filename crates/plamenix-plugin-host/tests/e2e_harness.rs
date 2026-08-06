//! End-to-end test harness (I9.1).
//!
//! Reusable scaffolding for "load → activate → assert contributions
//! registered → deactivate" scenarios. Subsequent I9 items
//! (I9.3 capability enforcement, I9.4 crash isolation, I9.5 targets
//! enforcement) layer extra assertions on top of the same harness so
//! the integration tests stay tight + uniformly structured.
//!
//! ## Pipeline
//!
//! 1. **Stage** — write a manifest + the hello-plugin wasm fixture
//!    to a temp directory.
//! 2. **Load** — `plamenix_plugin_host::load` walks the dir and
//!    returns a `StagedPlugin` with parsed manifest + component
//!    bytes.
//! 3. **Supervisor register** — `Supervisor::register` brings the
//!    plugin into the `Loaded` state.
//! 4. **Activate into registry** — `activate_into_registry` builds
//!    the wasmtime store, runs `instantiate_async` + `call_activate`,
//!    inserts a live `PluginInstance` into the registry.
//! 5. **Register contributions** — `register_event_subscriptions`
//!    wires every `[[contributions.event_subscriptions]]` entry
//!    against the shared `EventBus`.
//! 6. **Assert** — caller checks the supervisor + instance registry +
//!    event-bus subscription counts.
//! 7. **Deactivate** — `unregister_event_subscriptions` +
//!    `InstanceRegistry::remove` + `Supervisor::mark_stopped`.
//!
//! The harness functions are `pub` within this test crate so the
//! later I9 items can call them; integration-tests compile each
//! `tests/*.rs` file as a separate crate so this file IS the
//! harness, not a `mod`-included file.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use plamenix_plugin_host::{
    ActivationOutcome, EventBus, HostState, InstanceRegistry, PluginHost, PluginStatus,
    RestartPolicy, StagedPlugin, Supervisor, activate_into_registry, load,
    register_event_subscriptions, unregister_event_subscriptions,
};
use semver::Version;

const HELLO_WASM_BYTES: &[u8] = include_bytes!("fixtures/hello-plugin.wasm");

/// Minimal manifest with no contributions. Used by tests that don't
/// care about the contribution surface.
pub const HELLO_MINIMAL_MANIFEST: &str = r#"
[plugin]
id = "org.plamenix.hello"
name = "Hello"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"
"#;

/// Manifest declaring two event subscriptions so contribution-count
/// assertions have something to verify.
pub const HELLO_WITH_SUBSCRIPTIONS: &str = r#"
[plugin]
id = "org.plamenix.hello"
name = "Hello"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"

[contributions]
event_subscriptions = ["tab/opened", "query/executed"]
"#;

/// Writes the hello-plugin wasm + supplied manifest into `dir`.
pub fn stage_hello_bundle(dir: &Path, manifest: &str) {
    std::fs::write(dir.join("plugin.wasm"), HELLO_WASM_BYTES).unwrap();
    std::fs::write(dir.join("manifest.toml"), manifest).unwrap();
}

/// Loads a staged plugin from `bundle_dir`. Helper that pins the
/// host version + maps lib errors to the test crate's panic-on-err
/// convention.
pub fn load_staged(host: &PluginHost, bundle_dir: &Path) -> StagedPlugin {
    load(host, &Version::parse("1.0.0-beta").unwrap(), bundle_dir).unwrap()
}

/// Loads, registers with the supervisor, activates into the
/// registry, and wires every event subscription. Returns the
/// staged plugin so the caller can read its manifest fields.
pub async fn load_and_activate(
    host: &PluginHost,
    bundle_dir: &Path,
    supervisor: &Supervisor,
    registry: &InstanceRegistry,
    bus: &Arc<EventBus>,
) -> StagedPlugin {
    let staged = load_staged(host, bundle_dir);
    supervisor
        .register(
            &staged.manifest.plugin.id,
            staged.manifest.plugin.restart_policy,
        )
        .unwrap();
    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    let outcome = activate_into_registry(host, state, &staged, registry)
        .await
        .unwrap();
    assert!(matches!(outcome, ActivationOutcome::Ok));
    supervisor
        .mark_active(&staged.manifest.plugin.id, std::time::Instant::now())
        .unwrap();
    let _ = register_event_subscriptions(bus, &staged).unwrap();
    staged
}

/// Returns the bus's total subscription count. Tests create fresh
/// buses per scenario so this equals the active plugin's subs.
pub fn bus_subscription_count(bus: &Arc<EventBus>) -> usize {
    bus.subscription_count()
}

/// Unregisters event subscriptions, removes the instance, and marks
/// the supervisor `Stopped` (user-requested deactivation). Returns
/// the count of subscriptions removed for the caller's assertions.
pub fn deactivate(
    plugin_id: &str,
    supervisor: &Supervisor,
    registry: &InstanceRegistry,
    bus: &Arc<EventBus>,
) -> usize {
    let removed = unregister_event_subscriptions(bus, plugin_id);
    let _ = registry.remove(plugin_id).unwrap();
    supervisor.mark_stopped(plugin_id).unwrap();
    removed
}

// ============================================================
// Tests exercising the harness with real plugin fixtures.
// ============================================================

#[tokio::test(flavor = "multi_thread")]
async fn e2e_load_activate_assert_deactivate() {
    let host = PluginHost::new().unwrap();
    let supervisor = Supervisor::new();
    let registry = InstanceRegistry::new();
    let bus = Arc::new(EventBus::new());

    let dir = tempfile::tempdir().unwrap();
    stage_hello_bundle(dir.path(), HELLO_WITH_SUBSCRIPTIONS);

    let staged = load_and_activate(&host, dir.path(), &supervisor, &registry, &bus).await;
    let plugin_id = staged.manifest.plugin.id.clone();

    // Supervisor state.
    assert_eq!(
        supervisor.status(&plugin_id).unwrap(),
        Some(PluginStatus::Active),
    );

    // Instance registry holds a live entry.
    assert!(registry.contains(&plugin_id).unwrap());
    assert_eq!(registry.len().unwrap(), 1);
    let instance = registry.get(&plugin_id).unwrap().unwrap();
    assert_eq!(instance.plugin_id(), plugin_id);

    // Event-bus subscriptions registered.
    assert_eq!(bus_subscription_count(&bus), 2);

    // Now deactivate.
    deactivate(&plugin_id, &supervisor, &registry, &bus);

    assert_eq!(
        supervisor.status(&plugin_id).unwrap(),
        Some(PluginStatus::Stopped),
    );
    assert!(!registry.contains(&plugin_id).unwrap());
    assert_eq!(registry.len().unwrap(), 0);
    assert_eq!(bus_subscription_count(&bus), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_deactivate_returns_removed_subscription_count() {
    let host = PluginHost::new().unwrap();
    let supervisor = Supervisor::new();
    let registry = InstanceRegistry::new();
    let bus = Arc::new(EventBus::new());
    let dir = tempfile::tempdir().unwrap();
    stage_hello_bundle(dir.path(), HELLO_WITH_SUBSCRIPTIONS);
    let staged = load_and_activate(&host, dir.path(), &supervisor, &registry, &bus).await;
    let removed = deactivate(&staged.manifest.plugin.id, &supervisor, &registry, &bus);
    assert_eq!(removed, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_minimal_manifest_has_zero_event_subscriptions() {
    let host = PluginHost::new().unwrap();
    let supervisor = Supervisor::new();
    let registry = InstanceRegistry::new();
    let bus = Arc::new(EventBus::new());

    let dir = tempfile::tempdir().unwrap();
    stage_hello_bundle(dir.path(), HELLO_MINIMAL_MANIFEST);
    let staged = load_and_activate(&host, dir.path(), &supervisor, &registry, &bus).await;

    let plugin_id = staged.manifest.plugin.id.clone();
    assert_eq!(bus_subscription_count(&bus), 0);
    // Still active in the registry + supervisor.
    assert!(registry.contains(&plugin_id).unwrap());
    assert_eq!(
        supervisor.status(&plugin_id).unwrap(),
        Some(PluginStatus::Active),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_re_activate_after_deactivate_clean_state() {
    let host = PluginHost::new().unwrap();
    let supervisor = Supervisor::new();
    let registry = InstanceRegistry::new();
    let bus = Arc::new(EventBus::new());

    let dir = tempfile::tempdir().unwrap();
    stage_hello_bundle(dir.path(), HELLO_WITH_SUBSCRIPTIONS);

    let staged = load_and_activate(&host, dir.path(), &supervisor, &registry, &bus).await;
    let plugin_id = staged.manifest.plugin.id.clone();
    deactivate(&plugin_id, &supervisor, &registry, &bus);

    // Re-activate against the same supervisor + registry.
    let _ = load_and_activate(&host, dir.path(), &supervisor, &registry, &bus).await;

    assert!(registry.contains(&plugin_id).unwrap());
    assert_eq!(
        supervisor.status(&plugin_id).unwrap(),
        Some(PluginStatus::Active),
    );
    // Subscription count is back to 2 (re-registered).
    assert_eq!(bus_subscription_count(&bus), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_default_restart_policy_records_transient() {
    let host = PluginHost::new().unwrap();
    let supervisor = Supervisor::new();
    let registry = InstanceRegistry::new();
    let bus = Arc::new(EventBus::new());

    let dir = tempfile::tempdir().unwrap();
    stage_hello_bundle(dir.path(), HELLO_MINIMAL_MANIFEST);
    let staged = load_and_activate(&host, dir.path(), &supervisor, &registry, &bus).await;

    let state = supervisor
        .state(&staged.manifest.plugin.id)
        .unwrap()
        .unwrap();
    assert_eq!(state.restart_policy, RestartPolicy::Transient);
}
