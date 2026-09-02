//! Integration coverage for `InstanceRegistry` (I8.2).
//!
//! Unit tests in `src/instance.rs` only exercise the empty-registry
//! paths because [`PluginMinimal`] cannot be constructed without a
//! real component. This test loads the existing `hello-plugin.wasm`
//! fixture, activates through `activate_into_registry`, then drives
//! the registry's bookkeeping (insert via activator, lookup, remove,
//! drop semantics).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_plugin_host::{
    ActivationOutcome, HostState, InstanceRegistry, PluginHost, activate_into_registry, load,
};
use semver::Version;

const HELLO_WASM_BYTES: &[u8] = include_bytes!("fixtures/hello-plugin.wasm");

fn write_hello_plugin(dir: &std::path::Path) {
    std::fs::write(dir.join("plugin.wasm"), HELLO_WASM_BYTES).unwrap();
    let manifest = r#"
[plugin]
id = "org.plamenix.hello"
name = "Hello"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"
"#;
    std::fs::write(dir.join("manifest.toml"), manifest).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn activate_into_registry_inserts_live_instance() {
    let host = PluginHost::new().expect("engine");
    let dir = tempfile::tempdir().unwrap();
    write_hello_plugin(dir.path());
    let staged = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path()).unwrap();

    let registry = InstanceRegistry::new();
    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    let outcome = activate_into_registry(&host, state, &staged, &registry)
        .await
        .unwrap();
    assert!(matches!(outcome, ActivationOutcome::Ok));

    assert_eq!(registry.len().unwrap(), 1);
    assert!(registry.contains("org.plamenix.hello").unwrap());
    let ids = registry.ids().unwrap();
    assert_eq!(ids, vec!["org.plamenix.hello".to_string()]);

    let instance = registry.get("org.plamenix.hello").unwrap().unwrap();
    assert_eq!(instance.plugin_id(), "org.plamenix.hello");
    let _ = instance.bindings();
}

#[tokio::test(flavor = "multi_thread")]
async fn ui_only_plugin_skips_registry_insert() {
    let host = PluginHost::new().expect("engine");
    let dir = tempfile::tempdir().unwrap();
    let manifest = r#"
[plugin]
id = "ui.only"
name = "UI Only"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"
"#;
    std::fs::write(dir.path().join("manifest.toml"), manifest).unwrap();

    let staged = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path()).unwrap();
    let registry = InstanceRegistry::new();
    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    let outcome = activate_into_registry(&host, state, &staged, &registry)
        .await
        .unwrap();
    assert!(matches!(outcome, ActivationOutcome::Ok));
    assert_eq!(registry.len().unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_returns_instance_and_drops_store() {
    let host = PluginHost::new().expect("engine");
    let dir = tempfile::tempdir().unwrap();
    write_hello_plugin(dir.path());
    let staged = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path()).unwrap();

    let registry = InstanceRegistry::new();
    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    activate_into_registry(&host, state, &staged, &registry)
        .await
        .unwrap();

    let removed = registry.remove("org.plamenix.hello").unwrap();
    assert!(removed.is_some());
    assert_eq!(registry.len().unwrap(), 0);
    assert!(!registry.contains("org.plamenix.hello").unwrap());

    // Dropping the returned Arc releases the store.
    drop(removed);
}

#[tokio::test(flavor = "multi_thread")]
async fn re_activate_replaces_previous_instance() {
    let host = PluginHost::new().expect("engine");
    let dir = tempfile::tempdir().unwrap();
    write_hello_plugin(dir.path());
    let staged = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path()).unwrap();

    let registry = InstanceRegistry::new();

    let state1 = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    activate_into_registry(&host, state1, &staged, &registry)
        .await
        .unwrap();
    let first = registry.get("org.plamenix.hello").unwrap().unwrap();
    let first_addr = std::sync::Arc::as_ptr(&first) as usize;
    drop(first);

    let state2 = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    activate_into_registry(&host, state2, &staged, &registry)
        .await
        .unwrap();
    let second = registry.get("org.plamenix.hello").unwrap().unwrap();
    let second_addr = std::sync::Arc::as_ptr(&second) as usize;

    // Different Arc => different PluginInstance value
    assert_ne!(first_addr, second_addr);
    assert_eq!(registry.len().unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn clear_releases_every_instance() {
    let host = PluginHost::new().expect("engine");
    let dir = tempfile::tempdir().unwrap();
    write_hello_plugin(dir.path());
    let staged = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path()).unwrap();

    let registry = InstanceRegistry::new();
    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    activate_into_registry(&host, state, &staged, &registry)
        .await
        .unwrap();
    assert_eq!(registry.len().unwrap(), 1);

    registry.clear().unwrap();
    assert_eq!(registry.len().unwrap(), 0);
    assert!(registry.get("org.plamenix.hello").unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_store_returns_an_async_guard() {
    let host = PluginHost::new().expect("engine");
    let dir = tempfile::tempdir().unwrap();
    write_hello_plugin(dir.path());
    let staged = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path()).unwrap();

    let registry = InstanceRegistry::new();
    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    activate_into_registry(&host, state, &staged, &registry)
        .await
        .unwrap();

    let instance = registry.get("org.plamenix.hello").unwrap().unwrap();
    // Lock + immediately drop; verifies the mutex grants an
    // exclusive guard without deadlock on the first lock.
    let guard = instance.lock_store().await;
    drop(guard);
}

/// The property the registry exists for: after `activate` returns, the
/// plugin is still callable in the same store.
///
/// Before this was wired, the activator dropped its `Store` the moment
/// activation finished. Every registry test above would still have
/// passed against that — they check the map, not whether the thing in
/// it can be called. Dispatching a second call is what distinguishes a
/// live instance from a parked handle.
#[tokio::test]
async fn a_registered_instance_is_still_callable_after_activation() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_hello_plugin(dir.path());

    let host = PluginHost::new().expect("host");
    let version = Version::parse("1.0.0-beta").expect("version");
    let staged = load(&host, &version, dir.path()).expect("load");
    let registry = InstanceRegistry::new();

    let outcome = activate_into_registry(
        &host,
        HostState::new(&staged.manifest.plugin.id, "1.0.0-beta"),
        &staged,
        &registry,
    )
    .await
    .expect("activate");
    assert!(matches!(outcome, ActivationOutcome::Ok));

    let instance = registry
        .get(&staged.manifest.plugin.id)
        .expect("registry lock")
        .expect("instance registered");

    // Reach back into the same store the activation ran in and dispatch
    // another call. This is exactly what event dispatch will do.
    let mut store = instance.lock_store().await;
    instance
        .bindings()
        .plamenix_plugin_plugin()
        .call_handle_event(&mut *store, "plamenix.test", "{}")
        .await
        .expect("the plugin's store must still be usable after activation");
}

/// Dropping the registry releases the stores it holds.
///
/// The isolation property the supervisor and the resource limits both
/// assume: nothing survives the registry.
#[tokio::test]
async fn dropping_the_registry_releases_its_instances() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_hello_plugin(dir.path());

    let host = PluginHost::new().expect("host");
    let version = Version::parse("1.0.0-beta").expect("version");
    let staged = load(&host, &version, dir.path()).expect("load");

    let instance = {
        let registry = InstanceRegistry::new();
        activate_into_registry(
            &host,
            HostState::new(&staged.manifest.plugin.id, "1.0.0-beta"),
            &staged,
            &registry,
        )
        .await
        .expect("activate");
        registry
            .get(&staged.manifest.plugin.id)
            .expect("registry lock")
            .expect("instance registered")
    };

    // The registry is gone; the only remaining reference is this one,
    // so dropping it drops the store too.
    assert_eq!(std::sync::Arc::strong_count(&instance), 1);
}
