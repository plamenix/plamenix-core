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
requires_subprocess = false
"#;

#[test]
fn parses_valid_manifest() {
    let manifest = Manifest::parse(VALID_MANIFEST).expect("valid manifest");
    assert_eq!(manifest.plugin.id, "org.example.csv-exporter");
    assert_eq!(manifest.plugin.version.to_string(), "1.0.0");
    assert!(
        manifest
            .permissions
            .required
            .contains(&Permission::DbReadAny)
    );
    assert!(
        manifest
            .permissions
            .required
            .contains(&Permission::ExportFormat)
    );
    assert!(matches!(
        manifest.permissions.optional.first(),
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
    assert_eq!(
        Permission::parse("runtime.subprocess").unwrap(),
        Permission::RuntimeSubprocess,
    );
}

#[test]
fn permission_set_grants_lookup_covers_both_buckets() {
    let manifest = Manifest::parse(VALID_MANIFEST).unwrap();
    assert!(manifest.permissions.grants(&Permission::DbReadAny));
    assert!(
        manifest
            .permissions
            .grants(&Permission::NetHttpsHost("api.example.com".into()))
    );
    assert!(!manifest.permissions.grants(&Permission::DbWriteAny));
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
