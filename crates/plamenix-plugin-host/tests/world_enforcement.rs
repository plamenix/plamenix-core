//! Does declaring a world actually constrain anything?
//!
//! `wit/plamenix.wit` says the host refuses unknown worlds, links only
//! what the declared world exposes, and cross-checks capability grants
//! against it — "Object Capability Model by construction". None of that
//! was enforced. `validate_world_identifier` checked punctuation, so a
//! manifest could name a world that does not exist, or name
//! `plugin-minimal` (which imports nothing but `host`) while requesting
//! write access to the database, and the host would accept both.
//!
//! These go through `Manifest::parse` and `activate`, not through the
//! world module's own unit tests, because the question is whether the
//! enforcement is actually on the path a plugin takes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use plamenix_plugin_host::{Manifest, PluginWorld};

fn manifest_with(plugin_extra: &str, permissions: &str) -> String {
    format!(
        r#"
[plugin]
id = "dev.plamenix.fixture"
name = "Fixture"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"
{plugin_extra}

[entry_points]
wasm = "plugin.wasm"

{permissions}
"#
    )
}

#[test]
fn the_default_world_is_minimal_and_resolves() {
    let manifest = Manifest::parse(&manifest_with("", "")).expect("parse");
    assert_eq!(manifest.plugin.world_tier, PluginWorld::Minimal);
}

#[test]
fn a_world_that_does_not_exist_is_refused_at_parse() {
    // Syntactically flawless and completely fictional. This is the case
    // that used to sail through and fail later as a linker error.
    let err = Manifest::parse(&manifest_with(
        r#"world = "plamenix:plugin@1.0.0/plugin-omnipotent""#,
        "",
    ))
    .expect_err("a fictional world must not parse");
    assert!(
        err.to_string().contains("plugin-omnipotent"),
        "the refusal should name what was wrong: {err}",
    );
}

#[test]
fn a_capability_the_world_cannot_reach_is_refused() {
    // The claim under test: a minimal plugin cannot hold db:write. Not
    // "cannot use" — cannot *hold*, because a granted capability the
    // plugin can never exercise is a permission prompt for nothing.
    let err = Manifest::parse(&manifest_with(
        "",
        "[permissions]\nrequired = [\"db.write.any\"]",
    ))
    .expect_err("minimal must not be able to request db.write");
    assert!(
        err.to_string().contains("plugin-db-writer"),
        "the refusal should say which world would work: {err}",
    );
}

#[test]
fn the_same_capability_is_accepted_by_a_world_that_exposes_it() {
    // The control. Without it, the test above would also pass against a
    // host that refuses every capability.
    let manifest = Manifest::parse(&manifest_with(
        r#"world = "plamenix:plugin@1.0.0/plugin-db-writer""#,
        "[permissions]\nrequired = [\"db.write.any\"]",
    ))
    .expect("db-writer should be allowed to request db.write");
    assert_eq!(manifest.plugin.world_tier, PluginWorld::DbWriter);
}

#[test]
fn a_desktop_only_world_may_not_claim_to_run_on_web() {
    let err = Manifest::parse(&manifest_with(
        "world = \"plamenix:plugin@1.0.0/plugin-integrated-desktop\"\ntargets = [\"desktop\", \"web\"]",
        "[permissions]\nrequired = [\"auth.os.macos\"]",
    ))
    .expect_err("keyring access cannot exist on web");
    assert!(err.to_string().contains("web edition"));
}

/// A component that imports more than its declared world exposes must
/// be refused, and told which world would have worked.
///
/// This replaces a test that asserted the host refused every world
/// above `plugin-minimal`, which it did because none of the higher
/// tiers had an implementation. They do now, so the claim under test
/// changed shape: the question is no longer "can the host link this"
/// but "does the declared world bound what the plugin can reach".
///
/// The fixture's manifest says `plugin-minimal` while its wasm imports
/// `plamenix:plugin/db`. Nothing about it is malicious — it is what an
/// author gets by adding a feature and forgetting to raise the world.
#[tokio::test]
async fn a_component_importing_more_than_its_world_is_refused() {
    let host = plamenix_plugin_host::PluginHost::new().expect("host");
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("manifest.toml"),
        manifest_with(r#"world = "plamenix:plugin@1.0.0/plugin-minimal""#, ""),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("plugin.wasm"),
        include_bytes!("fixtures/over-reaching-plugin.wasm"),
    )
    .unwrap();

    let staged = plamenix_plugin_host::load(
        &host,
        &semver::Version::parse("1.0.0-beta").unwrap(),
        dir.path(),
    )
    .expect("staging only compiles the component; the refusal is at instantiation");

    let err = plamenix_plugin_host::activate(&host, "1.0.0-beta", &staged)
        .await
        .expect_err("a minimal-world plugin must not reach the db interface");

    let message = err.to_string();
    assert!(
        message.contains("plamenix:plugin/db"),
        "the refusal should name the import that was not allowed: {message}",
    );
    assert!(
        !message.contains("not found in the linker"),
        "wasmtime's own wording reads as a broken host; it should have been translated: {message}",
    );
    assert!(
        message.contains("plugin-db-reader"),
        "and the world that would have worked: {message}",
    );
}

/// The same fixture, declaring the world it actually needs, activates.
///
/// Without this the test above would pass against a host that refused
/// every plugin.
#[tokio::test]
async fn the_same_component_activates_once_it_declares_the_right_world() {
    let host = plamenix_plugin_host::PluginHost::new().expect("host");
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("manifest.toml"),
        manifest_with(r#"world = "plamenix:plugin@1.0.0/plugin-db-reader""#, ""),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("plugin.wasm"),
        include_bytes!("fixtures/over-reaching-plugin.wasm"),
    )
    .unwrap();

    let staged = plamenix_plugin_host::load(
        &host,
        &semver::Version::parse("1.0.0-beta").unwrap(),
        dir.path(),
    )
    .expect("load");

    plamenix_plugin_host::activate(&host, "1.0.0-beta", &staged)
        .await
        .expect("declaring plugin-db-reader should be enough to reach the db interface");
}

#[test]
fn a_ui_only_plugin_needs_no_linking() {
    // There is nothing to instantiate, so the declared world costs
    // nothing to satisfy. Refusing here would block a perfectly valid
    // UI-only plugin over imports it never uses.
    let host = plamenix_plugin_host::PluginHost::new().expect("host");
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("manifest.toml"),
        r#"
[plugin]
id = "dev.plamenix.uionly"
name = "UI Only"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"
world = "plamenix:plugin@1.0.0/plugin-integrated"

[entry_points]
ui = "index.mjs"
"#,
    )
    .unwrap();

    plamenix_plugin_host::load(
        &host,
        &semver::Version::parse("1.0.0-beta").unwrap(),
        dir.path(),
    )
    .expect("a UI-only plugin has nothing to link");
}
