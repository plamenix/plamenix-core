//! I9.3 — capability enforcement tests.
//!
//! Verifies that a plugin requesting an ungranted capability fails
//! cleanly with a typed error, not a panic or a silent allow.
//!
//! ## M1 enforcement surface
//!
//! Every entry in `[permissions]` parses through the capability
//! grammar (`capability.rs`). Garbage strings fail at parse time as
//! [`PluginError::InvalidCapability`], so a manifest cannot smuggle an
//! unrecognised capability past the host by misspelling it.
//!
//! Per-host-import permission gates (e.g. `host.fs.read(path)`
//! checking against `fs.read:<glob>`) are M2 work — today's host
//! imports (`log`, `host-version`, `edition`) are universal
//! capabilities. The M1 deliverable is "what IS enforced is
//! correct + surfaces typed errors", not "every conceivable
//! capability gate exists".

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use plamenix_plugin_host::{Manifest, Permission, PermissionSet, PluginError, PluginHost, load};
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
    assert!(!set.grants(&Permission::ClipboardWrite));
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
