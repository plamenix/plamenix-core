//! One set of bindings has to serve all five worlds. Does it?
//!
//! `src/bindings.rs` generates against `plugin-integrated-desktop`, the
//! largest world, and every plugin — whatever tier it declared — is
//! instantiated and called through that one type. The alternative was
//! five `bindgen!` invocations minting five distinct Rust types for the
//! same interface, which would force [`PluginInstance`] to become
//! generic over the world and ripple through the registry, the
//! dispatcher, and the interceptor path.
//!
//! Soundness rests on two properties of wasmtime that are easy to
//! assume and worth pinning down:
//!
//! * `Linker::typecheck` walks the *component's* import list, so a
//!   linker holding more than the component needs is not an error.
//! * The generated world struct resolves exports only, and all five
//!   worlds export an identical `plamenix:plugin/plugin`.
//!
//! `hello-plugin` is built against `plugin-minimal`. If either property
//! stopped holding, activating it here would fail — which is the whole
//! point of this file.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use plamenix_plugin_host::{
    ActivationOutcome, DispatchOutcome, EventBus, HostState, InstanceRegistry, PluginHost,
    activate_into_registry, dispatch_event, load,
};
use semver::Version;

const HELLO_WASM: &[u8] = include_bytes!("fixtures/hello-plugin.wasm");
const PLUGIN_ID: &str = "dev.plamenix.tiers";

fn stage(dir: &std::path::Path) {
    std::fs::write(dir.join("plugin.wasm"), HELLO_WASM).unwrap();
    std::fs::write(
        dir.join("manifest.toml"),
        r#"
[plugin]
id = "dev.plamenix.tiers"
name = "Tier Fixture"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"
world = "plamenix:plugin@1.0.0/plugin-minimal"

[entry_points]
wasm = "plugin.wasm"
"#,
    )
    .unwrap();
}

#[tokio::test]
async fn a_minimal_component_activates_through_the_largest_worlds_bindings() {
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path());

    let staged = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path()).expect("load");
    let outcome = activate_into_registry(
        &host,
        HostState::new(PLUGIN_ID, "1.0.0-beta"),
        &staged,
        &registry,
    )
    .await
    .expect("a plugin-minimal component must instantiate through the integrated bindings");

    assert!(
        matches!(outcome, ActivationOutcome::Ok),
        "activation failed: {outcome:?}",
    );
}

#[tokio::test]
async fn the_exports_still_resolve_after_the_world_swap() {
    // Activation only proves the imports linked. This proves the export
    // side of the claim: the `plamenix:plugin/plugin` accessor found on
    // the integrated-desktop world struct really does reach a component
    // built against plugin-minimal.
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path());

    let staged = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path()).expect("load");
    activate_into_registry(
        &host,
        HostState::new(PLUGIN_ID, "1.0.0-beta"),
        &staged,
        &registry,
    )
    .await
    .expect("activate");

    let bus = EventBus::new();
    bus.subscribe(PLUGIN_ID, "**").expect("subscribe");
    let deliveries = dispatch_event(&bus, &registry, "db/query/executed", "{}").await;

    assert_eq!(deliveries[0].outcome, DispatchOutcome::Delivered);
}
