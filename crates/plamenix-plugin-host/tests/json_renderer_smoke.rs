//! I9.2 — per-edition smoke test for the JSON cell renderer plugin.
//!
//! The JSON cell renderer is a **UI-only** bundle: it ships only a
//! `ui.mjs` module + a `manifest.toml`. There's no wasm half, so the
//! activator skips the wasmtime instantiation path and returns
//! `ActivationOutcome::Ok` without touching the engine. Despite the
//! lack of wasm, the bundle is a tri-state validator for the plugin
//! pipeline:
//!
//!   1. **Manifest parsing** — `targets = ["desktop", "web"]` +
//!      `[entry_points] ui = "ui.mjs"` exercise the manifest loader's
//!      cross-edition path.
//!   2. **Edition check** — the host's `Edition::supports_edition`
//!      call must accept the same `.plx` on both editions.
//!   3. **Activator UI-only branch** — `activate_with_state`
//!      short-circuits to `Ok` when `staged.component` is `None`.
//!
//! These tests stage the SAME bundle layout under a tempdir and
//! activate it twice: once with `HostState::new` (defaults to
//! `"desktop"`) and once with `HostState::with_edition("web")`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use plamenix_plugin_host::{
    ActivationOutcome, Edition, HostState, InstanceRegistry, PluginHost,
    activate_into_registry, activate_with_state, load,
};
use semver::Version;

/// Minimal copy of the JSON cell renderer's manifest. Carries the
/// `targets = ["desktop", "web"]` + UI-only entry that I9.2 needs to
/// exercise.
const MANIFEST: &str = r#"
[plugin]
id = "dev.plamenix.json-cell-renderer"
name = "JSON Cell Renderer"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"
targets = ["desktop", "web"]
restart_policy = "transient"

[permissions]
required = []
optional = []

[entry_points]
ui = "ui.mjs"
"#;

/// Stub UI module. The activator never reads this — `ui.mjs` is loaded
/// by the shell's React side at runtime, not by the Rust activator.
/// Present here so the bundle layout is realistic on disk.
const UI_STUB: &str = "// stub ui.mjs for the JSON cell renderer smoke test\nexport default function activate() {}\n";

fn stage_bundle(dir: &Path) {
    std::fs::write(dir.join("manifest.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("ui.mjs"), UI_STUB).unwrap();
}

fn host_version() -> Version {
    Version::parse("1.0.0-beta").unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn desktop_edition_activates_json_renderer_bundle() {
    let host = PluginHost::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    stage_bundle(dir.path());
    let staged = load(&host, &host_version(), dir.path()).unwrap();

    assert_eq!(staged.manifest.plugin.id, "dev.plamenix.json-cell-renderer");
    assert!(staged.manifest.plugin.supports_edition(Edition::Desktop));
    // UI-only bundles have no component bytes.
    assert!(staged.component.is_none());

    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    let outcome = activate_with_state(&host, state, &staged).await.unwrap();
    assert!(
        matches!(outcome, ActivationOutcome::Ok),
        "expected Ok, got {outcome:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn web_edition_activates_json_renderer_bundle() {
    let host = PluginHost::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    stage_bundle(dir.path());
    let staged = load(&host, &host_version(), dir.path()).unwrap();

    assert_eq!(staged.manifest.plugin.id, "dev.plamenix.json-cell-renderer");
    assert!(staged.manifest.plugin.supports_edition(Edition::Web));
    assert!(staged.component.is_none());

    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta")
        .with_edition("web");
    let outcome = activate_with_state(&host, state, &staged).await.unwrap();
    assert!(
        matches!(outcome, ActivationOutcome::Ok),
        "expected Ok, got {outcome:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn activate_into_registry_skips_registry_for_ui_only_bundles() {
    let host = PluginHost::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    stage_bundle(dir.path());
    let staged = load(&host, &host_version(), dir.path()).unwrap();

    let registry = InstanceRegistry::new();
    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    let outcome = activate_into_registry(&host, state, &staged, &registry)
        .await
        .unwrap();
    assert!(matches!(outcome, ActivationOutcome::Ok));
    // UI-only bundles aren't recorded in the registry — there's no
    // wasmtime store to track. Behaviour shared with the existing
    // instance-registry integration test, but verified here for the
    // JSON renderer's specific shape so future per-bundle smoke
    // tests catch a regression in the activator's branching.
    assert_eq!(registry.len().unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn manifest_targets_block_unlisted_editions() {
    // Stage a JSON renderer manifest restricted to web only +
    // verify desktop install is refused.
    let restricted = MANIFEST.replace(
        r#"targets = ["desktop", "web"]"#,
        r#"targets = ["web"]"#,
    );
    let host = PluginHost::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("manifest.toml"), restricted).unwrap();
    std::fs::write(dir.path().join("ui.mjs"), UI_STUB).unwrap();
    let staged = load(&host, &host_version(), dir.path()).unwrap();
    assert!(staged.manifest.plugin.supports_edition(Edition::Web));
    assert!(!staged.manifest.plugin.supports_edition(Edition::Desktop));
}
