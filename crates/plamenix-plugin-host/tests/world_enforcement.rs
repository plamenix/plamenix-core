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
//! These go through `Manifest::parse`, not through the world module's
//! own unit tests, because the question is whether the enforcement is
//! actually on the path a plugin takes.

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

#[test]
fn a_world_the_host_cannot_link_is_refused_when_it_ships_wasm() {
    // Higher tiers are declared in the contract but their imports have
    // no host implementation, so instantiating one would fail with a
    // wasmtime linker error that reads like the plugin's fault.
    let host = plamenix_plugin_host::PluginHost::new().expect("host");
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("manifest.toml"),
        manifest_with(r#"world = "plamenix:plugin@1.0.0/plugin-db-reader""#, ""),
    )
    .unwrap();
    std::fs::write(dir.path().join("plugin.wasm"), b"\0asm\x01\0\0\0").unwrap();

    let err = plamenix_plugin_host::load(
        &host,
        &semver::Version::parse("1.0.0-beta").unwrap(),
        dir.path(),
    )
    .expect_err("an unlinkable world must be refused");
    let message = err.to_string();
    assert!(
        message.contains("no host implementation"),
        "the refusal must name the host as the gap, not the plugin: {message}",
    );
}

#[test]
fn a_ui_only_plugin_may_declare_a_world_the_host_cannot_link() {
    // There is nothing to link. Refusing here would block a perfectly
    // valid UI-only plugin over an import it never uses.
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
