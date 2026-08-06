//! I9.5 — `targets` enforcement tests.
//!
//! A plugin's `[plugin].targets` list declares which editions it
//! supports. The host MUST refuse to install / activate a plugin
//! whose declared targets exclude the current edition.
//!
//! ## Where enforcement lives
//!
//! - **Rust host (this crate)** ships `PluginMetadata::supports_edition(Edition)` —
//!   the predicate every host wrapper consults.
//! - **Web edition** (`plamenix-web/packages/server/src/plugins/host.ts`)
//!   calls the napi binding's exposed `staged.targets` field and
//!   skips activation on mismatch. Integration test for that path
//!   ships alongside the web server's existing
//!   `test/plugins/edition-mismatch.test.ts` (covered by I9.5 web
//!   sidecar).
//! - **Desktop edition** uses the same predicate via the Tauri host's
//!   bootstrap scanner. Desktop-edition integration coverage relies
//!   on the predicate being correct + the existing scanner unit
//!   tests; today's deliverable is the host-side predicate matrix +
//!   the web server's HTTP-level test.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_plugin_host::{Edition, Manifest};

fn manifest_with_targets(targets: &str) -> String {
    format!(
        r#"
[plugin]
id = "test.targets"
name = "Targets"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"
targets = {targets}

[entry_points]
ui = "ui.mjs"

[permissions]
required = []
optional = []
"#,
    )
}

#[test]
fn desktop_only_manifest_refuses_web_edition() {
    let m = Manifest::parse(&manifest_with_targets(r#"["desktop"]"#)).unwrap();
    assert!(m.plugin.supports_edition(Edition::Desktop));
    assert!(!m.plugin.supports_edition(Edition::Web));
}

#[test]
fn web_only_manifest_refuses_desktop_edition() {
    let m = Manifest::parse(&manifest_with_targets(r#"["web"]"#)).unwrap();
    assert!(m.plugin.supports_edition(Edition::Web));
    assert!(!m.plugin.supports_edition(Edition::Desktop));
}

#[test]
fn both_editions_manifest_supports_both() {
    let m = Manifest::parse(&manifest_with_targets(r#"["desktop", "web"]"#)).unwrap();
    assert!(m.plugin.supports_edition(Edition::Desktop));
    assert!(m.plugin.supports_edition(Edition::Web));
}

#[test]
fn targets_order_does_not_matter() {
    let m = Manifest::parse(&manifest_with_targets(r#"["web", "desktop"]"#)).unwrap();
    assert!(m.plugin.supports_edition(Edition::Desktop));
    assert!(m.plugin.supports_edition(Edition::Web));
}

#[test]
fn missing_targets_field_defaults_to_both_editions() {
    // Defaults documented at `PluginMetadata::targets`: "Defaults to
    // `[Desktop, Web]`." When the field is absent from TOML the
    // parser fills in the default; both editions remain supported.
    let m = Manifest::parse(
        r#"
[plugin]
id = "test.targets.default"
name = "Default Targets"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"

[permissions]
required = []
optional = []
"#,
    )
    .unwrap();
    assert!(m.plugin.supports_edition(Edition::Desktop));
    assert!(m.plugin.supports_edition(Edition::Web));
}

#[test]
fn unknown_target_string_fails_at_parse() {
    let err = Manifest::parse(&manifest_with_targets(r#"["mobile"]"#)).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("edition") || msg.to_lowercase().contains("target"),
        "expected parse error to mention edition / target, got: {msg}",
    );
}

#[test]
fn empty_targets_list_rejected_at_parse() {
    // An empty list would semantically opt out of every edition.
    // The parser short-circuits that pathological config with a
    // typed error so plugin authors don't accidentally ship an
    // un-installable bundle.
    let err = Manifest::parse(&manifest_with_targets("[]")).unwrap_err();
    let msg = format!("{err}").to_lowercase();
    assert!(
        msg.contains("targets") && (msg.contains("empty") || msg.contains("at least one")),
        "expected parse error to mention empty targets, got: {msg}",
    );
}
