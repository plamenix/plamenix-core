//! Offline tests for manifest parsing, capability grammar, and the
//! loader's validation pipeline (everything except actually running a
//! wasm component, which Phase B covers).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_plugin_host::{LogicalDir, Manifest, Permission, PluginError, PluginHost, load};
use semver::Version;

const VALID_MANIFEST: &str = r#"
[plugin]
id = "org.example.csv-exporter"
name = "CSV Exporter"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"
author = "Example <dev@example.org>"
license = "MIT OR Apache-2.0"

[permissions]
required = ["db.read.any", "export.format"]
optional = ["net.https.api.example.com"]

[entry_points]
ui = "dist/ui.mjs"

[runtime]
"#;

#[test]
fn parses_valid_manifest() {
    let manifest = Manifest::parse(VALID_MANIFEST).expect("valid manifest");
    assert_eq!(manifest.plugin.id, "org.example.csv-exporter");
    assert_eq!(manifest.plugin.version.to_string(), "1.0.0");
    assert!(
        manifest
            .permissions
            .required_caps()
            .any(|p| matches!(p, Permission::DbReadAny))
    );
    assert!(
        manifest
            .permissions
            .required_caps()
            .any(|p| matches!(p, Permission::ExportFormat))
    );
    assert!(matches!(
        manifest.permissions.optional.first().map(|g| &g.capability),
        Some(Permission::NetHttpsHost(host)) if host == "api.example.com"
    ));
}

#[test]
fn rejects_manifest_with_neither_entry_point() {
    let text = r#"
[plugin]
id = "x"
name = "X"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
"#;
    let err = Manifest::parse(text).expect_err("missing entry points should fail");
    assert!(
        matches!(err, PluginError::InvalidManifest(msg) if msg.contains("entry_points")),
        "unexpected variant",
    );
}

#[test]
fn rejects_unknown_capability() {
    let text = r#"
[plugin]
id = "x"
name = "X"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[permissions]
required = ["mystery.deep.access"]

[entry_points]
ui = "ui.mjs"
"#;
    let err = Manifest::parse(text).expect_err("unknown capability should fail");
    assert!(
        matches!(err, PluginError::InvalidCapability(cap, _) if cap == "mystery.deep.access"),
        "unexpected variant",
    );
}

#[test]
fn parses_scoped_capabilities() {
    assert_eq!(
        Permission::parse("db.read.table.users").unwrap(),
        Permission::DbReadTable("users".into()),
    );
    assert_eq!(
        Permission::parse("net.https.api.example.com").unwrap(),
        Permission::NetHttpsHost("api.example.com".into()),
    );
    assert_eq!(
        Permission::parse("fs.read.dir.downloads").unwrap(),
        Permission::FsReadDir(LogicalDir::Downloads),
    );
}

#[test]
fn parses_detailed_permission_grant_with_purpose() {
    let text = r#"
[plugin]
id = "org.example.detailed"
name = "Detailed"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[permissions]
required = [
  { capability = "db.schema.list", purpose = "List tables to choose from" },
  { capability = "fs.write.dir.plugin-data", purpose = "Cache export progress" },
]

[entry_points]
ui = "ui.mjs"
"#;
    let manifest = Manifest::parse(text).expect("valid detailed grants");
    assert_eq!(manifest.permissions.required.len(), 2);
    let first = &manifest.permissions.required[0];
    assert_eq!(first.capability, Permission::DbSchemaList);
    assert_eq!(first.purpose.as_deref(), Some("List tables to choose from"));
    let second = &manifest.permissions.required[1];
    assert_eq!(
        second.capability,
        Permission::FsWriteDir(LogicalDir::PluginData)
    );
    assert_eq!(second.purpose.as_deref(), Some("Cache export progress"));
}

#[test]
fn parses_mixed_plain_and_detailed_grants_in_same_list() {
    let text = r#"
[plugin]
id = "org.example.mixed"
name = "Mixed"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[permissions]
required = [
  "db.schema.list",
  { capability = "fs.write.dir.plugin-data", purpose = "Cache export progress" },
]

[entry_points]
ui = "ui.mjs"
"#;
    let manifest = Manifest::parse(text).expect("valid mixed grants");
    assert_eq!(manifest.permissions.required.len(), 2);
    // Plain-string form leaves purpose absent.
    assert_eq!(
        manifest.permissions.required[0].capability,
        Permission::DbSchemaList
    );
    assert!(manifest.permissions.required[0].purpose.is_none());
    // Detailed-object form attaches purpose.
    assert!(manifest.permissions.required[1].purpose.is_some());
}

#[test]
fn empty_purpose_string_treated_as_absent() {
    let text = r#"
[plugin]
id = "org.example.empty"
name = "Empty"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[permissions]
required = [{ capability = "db.schema.list", purpose = "" }]

[entry_points]
ui = "ui.mjs"
"#;
    let manifest = Manifest::parse(text).expect("valid manifest");
    // Empty-string purpose collapses to None, so a blank rationale and
    // an absent one are treated identically rather than showing the
    // user an empty line where an explanation should be.
    assert!(manifest.permissions.required[0].purpose.is_none());
}

#[test]
fn permission_set_grants_lookup_covers_both_buckets() {
    let manifest = Manifest::parse(VALID_MANIFEST).unwrap();
    assert!(manifest.permissions.declares(&Permission::DbReadAny));
    assert!(
        manifest
            .permissions
            .declares(&Permission::NetHttpsHost("api.example.com".into()))
    );
    assert!(!manifest.permissions.declares(&Permission::DbWriteAny));
}

#[test]
fn loader_reports_missing_manifest() {
    let host = PluginHost::new().expect("engine");
    let dir = tempfile::tempdir().expect("tempdir");
    let err = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path())
        .expect_err("missing manifest should fail");
    assert!(matches!(err, PluginError::ManifestMissing(_)));
}

#[test]
fn loader_reports_incompatible_host() {
    let host = PluginHost::new().expect("engine");
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = r#"
[plugin]
id = "x"
name = "X"
version = "1.0.0"
plamenix_min_version = ">=2.0.0"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"
"#;
    std::fs::write(dir.path().join("manifest.toml"), manifest).unwrap();

    let err = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path())
        .expect_err("host below required should fail");
    assert!(matches!(err, PluginError::IncompatibleHost { .. }));
}

#[test]
fn loader_accepts_ui_only_plugin() {
    let host = PluginHost::new().expect("engine");
    let dir = tempfile::tempdir().expect("tempdir");
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

    let staged = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path())
        .expect("ui-only plugin should load");
    assert_eq!(staged.manifest.plugin.id, "ui.only");
    assert!(staged.component.is_none());
}

#[test]
fn manifest_parses_event_subscriptions() {
    // I6.2 — manifests with `[contributions]
    // event_subscriptions = [...]` get parsed + validated. Valid
    // pattern grammar: slash-segmented with `*` / `**` wildcards.
    let manifest = r#"
[plugin]
id = "com.example.audit"
name = "Audit"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"

[contributions]
event_subscriptions = ["query/executed", "connection/**", "tab/*"]
"#;
    let parsed = Manifest::parse(manifest).expect("valid manifest");
    assert_eq!(
        parsed.contributions.event_subscriptions,
        vec!["query/executed", "connection/**", "tab/*"],
    );
}

#[test]
fn manifest_defaults_event_subscriptions_to_empty_when_absent() {
    let manifest = r#"
[plugin]
id = "com.example.silent"
name = "Silent"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"
"#;
    let parsed = Manifest::parse(manifest).expect("valid manifest");
    assert!(parsed.contributions.event_subscriptions.is_empty());
}

#[test]
fn manifest_rejects_empty_event_subscription_pattern() {
    // I6.2 — empty pattern string fails parse-time validation
    // (fail-loud beats failing silently at activation).
    let manifest = r#"
[plugin]
id = "com.example.broken"
name = "Broken"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"

[contributions]
event_subscriptions = [""]
"#;
    let err = Manifest::parse(manifest).expect_err("empty pattern must error");
    let msg = err.to_string();
    assert!(
        msg.contains("event_subscriptions") && msg.contains("non-empty"),
        "expected event_subscriptions validation error, got: {msg}",
    );
}
