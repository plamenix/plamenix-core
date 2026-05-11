//! Phase B integration test for [`activate`].
//!
//! Verifies the activation pipeline returns `Ok` for the ui-only path
//! (no wasm component to instantiate). A full end-to-end test with a
//! real Rust plugin compiled to `wasm32-wasip2` lands in Phase C; the
//! Component Model's WAT surface is too verbose to hand-author here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_plugin_host::{ActivationOutcome, PluginHost, activate, load};
use semver::Version;

fn host_version() -> Version {
    Version::parse("1.0.0-beta").unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn activate_ui_only_returns_ok_without_running_wasm() {
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

    let staged = load(&host, &host_version(), dir.path()).unwrap();
    let outcome = activate(&host, "1.0.0-beta", &staged).await.unwrap();
    assert!(matches!(outcome, ActivationOutcome::Ok));
}
