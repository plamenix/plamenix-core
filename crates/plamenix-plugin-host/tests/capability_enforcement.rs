//! I9.3 — capability enforcement tests.
//!
//! Verifies that a plugin requesting an ungranted capability fails
//! cleanly with a typed error, not a panic or a silent allow.
//!
//! ## M1 enforcement surface
//!
//! Today's host enforces capability rules at two points:
//!
//!   1. **Manifest parse time** — the manifest's `[runtime]
//!      requires_subprocess` flag must be paired with a
//!      `runtime.subprocess` capability in the manifest's
//!      `[permissions].required` set. Missing pairing →
//!      [`PluginError::MissingSubprocessCapability`].
//!   2. **Subprocess activator** — `activate_subprocess` re-checks the
//!      same capability before spawning the binary. Defense in depth
//!      against a future code path that constructs a `Manifest` via
//!      something other than `Manifest::parse`.
//!   3. **Capability-grammar** — every entry in `[permissions]` parses
//!      through the capability grammar (`capability.rs`). Garbage
//!      strings fail at parse time as
//!      [`PluginError::InvalidCapability`].
//!
//! Per-host-import permission gates (e.g. `host.fs.read(path)`
//! checking against `fs.read:<glob>`) are M2 work — today's host
//! imports (`log`, `host-version`, `edition`) are universal
//! capabilities. The M1 deliverable is "what IS enforced is
//! correct + surfaces typed errors", not "every conceivable
//! capability gate exists". The tracker entry for I9.3 calls this
//! out explicitly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use plamenix_plugin_host::{
    Manifest, Permission, PermissionSet, PluginError, PluginHost, activate_subprocess, load,
};
use semver::Version;

const HELLO_WASM_BYTES: &[u8] = include_bytes!("fixtures/hello-plugin.wasm");

fn stage_with_manifest(dir: &Path, manifest: &str) {
    std::fs::write(dir.join("plugin.wasm"), HELLO_WASM_BYTES).unwrap();
    std::fs::write(dir.join("manifest.toml"), manifest).unwrap();
}

fn host_version() -> Version {
    Version::parse("1.0.0-beta").unwrap()
}

#[test]
fn requires_subprocess_without_grant_fails_at_manifest_parse() {
    // `requires_subprocess = true` but no `runtime.subprocess` in
    // [permissions].required. Manifest parser must refuse.
    let manifest = r#"
[plugin]
id = "test.bad.subprocess"
name = "Bad Subprocess"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"
subprocess = "bin/plugin"

[runtime]
requires_subprocess = true

[permissions]
required = []
optional = []
"#;
    let err = Manifest::parse(manifest).unwrap_err();
    assert!(
        matches!(err, PluginError::MissingSubprocessCapability),
        "expected MissingSubprocessCapability, got {err:?}",
    );
}

#[test]
fn requires_subprocess_with_grant_parses_clean() {
    let manifest = r#"
[plugin]
id = "test.good.subprocess"
name = "Good Subprocess"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"
subprocess = "bin/plugin"

[runtime]
requires_subprocess = true

[permissions]
required = [{ capability = "runtime.subprocess" }]
optional = []
"#;
    let parsed = Manifest::parse(manifest).unwrap();
    assert!(parsed.permissions.grants(&Permission::RuntimeSubprocess));
    assert!(parsed.runtime.requires_subprocess);
}

#[test]
fn malformed_capability_string_rejected_at_parse_time() {
    let manifest = r#"
[plugin]
id = "test.bad.capability"
name = "Bad Capability"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"

[permissions]
required = [{ capability = "this.is.not.a.real.capability" }]
optional = []
"#;
    let err = Manifest::parse(manifest).unwrap_err();
    assert!(
        matches!(err, PluginError::InvalidCapability(_, _)),
        "expected InvalidCapability, got {err:?}",
    );
}

#[test]
fn empty_permission_set_grants_nothing() {
    let set = PermissionSet::default();
    assert!(!set.grants(&Permission::RuntimeSubprocess));
    assert!(!set.grants(&Permission::DbReadAny));
    assert!(!set.grants(&Permission::NetHttps));
}

#[test]
fn declared_permission_appears_in_grants() {
    let manifest = r#"
[plugin]
id = "test.granted"
name = "Granted"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"

[permissions]
required = [{ capability = "db.read.any" }]
optional = [{ capability = "net.https" }]
"#;
    let m = Manifest::parse(manifest).unwrap();
    assert!(m.permissions.grants(&Permission::DbReadAny));
    assert!(m.permissions.grants(&Permission::NetHttps));
    // Not declared, even though we asked for db.read.any.
    assert!(!m.permissions.grants(&Permission::DbWriteAny));
}

#[tokio::test(flavor = "multi_thread")]
async fn loader_propagates_invalid_capability_error() {
    let host = PluginHost::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let manifest = r#"
[plugin]
id = "test.bad.through.loader"
name = "Bad Through Loader"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"

[permissions]
required = [{ capability = "garbage.capability.name" }]
optional = []
"#;
    stage_with_manifest(dir.path(), manifest);
    let err = load(&host, &host_version(), dir.path()).unwrap_err();
    assert!(
        matches!(err, PluginError::InvalidCapability(_, _)),
        "expected InvalidCapability from loader, got {err:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn subprocess_activator_refuses_without_capability_grant() {
    // Defense-in-depth test: even when `activate_subprocess` is
    // called via a `StagedPlugin` whose manifest passed the parse
    // gate (because `requires_subprocess = false`), the activator
    // re-checks the capability before running the binary. Models the
    // future scenario where a caller routes around the manifest
    // parser to construct a `Manifest` programmatically.
    let host = PluginHost::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let manifest_text = r#"
[plugin]
id = "test.subprocess.defense"
name = "Subprocess Defense"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
subprocess = "never-run"

[runtime]
requires_subprocess = false

[permissions]
required = []
optional = []
"#;
    std::fs::write(dir.path().join("manifest.toml"), manifest_text).unwrap();
    // Stage a stub binary so the loader's exists-check doesn't bail
    // before the capability re-check runs.
    std::fs::write(dir.path().join("never-run"), b"").unwrap();
    let staged = load(&host, &host_version(), dir.path()).unwrap();
    let err = activate_subprocess("1.0.0-beta", &staged)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PluginError::MissingSubprocessCapability),
        "expected MissingSubprocessCapability, got {err:?}",
    );
}
