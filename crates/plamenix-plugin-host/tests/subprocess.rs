//! Integration tests for the subprocess activator.
//!
//! Manifest-validation tests run everywhere; activation tests use a
//! short POSIX shell script as the plugin binary and are therefore
//! `cfg(unix)`. Windows coverage will come once we ship a real
//! example plugin binary.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_plugin_host::{ActivationOutcome, Manifest, PluginError, PluginHost, activate, load};
use semver::Version;

fn host_version() -> Version {
    Version::parse("1.0.0-beta").unwrap()
}

#[test]
fn manifest_requires_subprocess_entry_when_flag_set() {
    let manifest = r#"
[plugin]
id = "needs.subprocess"
name = "Needs Subprocess"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[permissions]
required = ["runtime.subprocess"]

[entry_points]
ui = "ui.mjs"

[runtime]
requires_subprocess = true
"#;
    let err = Manifest::parse(manifest).unwrap_err();
    assert!(matches!(err, PluginError::MissingSubprocessEntry), "{err:?}");
}

#[test]
fn manifest_requires_subprocess_capability_when_flag_set() {
    let manifest = r#"
[plugin]
id = "needs.cap"
name = "Needs Capability"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
subprocess = "bin/plugin"

[runtime]
requires_subprocess = true
"#;
    let err = Manifest::parse(manifest).unwrap_err();
    assert!(matches!(err, PluginError::MissingSubprocessCapability), "{err:?}");
}

#[test]
fn manifest_accepts_subprocess_only_entry_when_flag_unset() {
    let manifest = r#"
[plugin]
id = "subprocess.only"
name = "Subprocess Only"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
subprocess = "bin/plugin"
"#;
    let parsed = Manifest::parse(manifest).unwrap();
    assert!(parsed.entry_points.subprocess.is_some());
    assert!(!parsed.runtime.requires_subprocess);
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn write_script(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let mut perm = std::fs::metadata(path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(path, perm).unwrap();
    }

    const MANIFEST_TEMPLATE: &str = r#"
[plugin]
id = "subprocess.test"
name = "Subprocess Test"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[permissions]
required = ["runtime.subprocess"]

[entry_points]
subprocess = "plugin.sh"

[runtime]
requires_subprocess = true
"#;

    fn stage_bundle(stdout_body: &str, exit_code: i32) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manifest.toml"), MANIFEST_TEMPLATE).unwrap();
        let script = format!("#!/bin/sh\nprintf '%s' '{stdout_body}'\nexit {exit_code}\n");
        write_script(&dir.path().join("plugin.sh"), &script);
        dir
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn activate_ok_returns_ok() {
        let host = PluginHost::new().unwrap();
        let dir = stage_bundle(r#"{"status":"ok"}"#, 0);
        let staged = load(&host, &host_version(), dir.path()).unwrap();
        let outcome = activate(&host, "1.0.0-beta", &staged).await.unwrap();
        assert!(matches!(outcome, ActivationOutcome::Ok));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn activate_failed_returns_failed_with_message() {
        let host = PluginHost::new().unwrap();
        let dir = stage_bundle(r#"{"status":"failed","message":"bad config"}"#, 0);
        let staged = load(&host, &host_version(), dir.path()).unwrap();
        let outcome = activate(&host, "1.0.0-beta", &staged).await.unwrap();
        match outcome {
            ActivationOutcome::Failed(msg) => assert_eq!(msg, "bad config"),
            ActivationOutcome::Ok => panic!("expected Failed, got Ok"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_zero_exit_surfaces_subprocess_failed() {
        let host = PluginHost::new().unwrap();
        let dir = stage_bundle(r#"{"status":"ok"}"#, 1);
        let staged = load(&host, &host_version(), dir.path()).unwrap();
        let err = activate(&host, "1.0.0-beta", &staged).await.unwrap_err();
        assert!(matches!(err, PluginError::SubprocessFailed { code: Some(1), .. }), "{err:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_stdout_surfaces_protocol_error() {
        let host = PluginHost::new().unwrap();
        let dir = stage_bundle("not json at all", 0);
        let staged = load(&host, &host_version(), dir.path()).unwrap();
        let err = activate(&host, "1.0.0-beta", &staged).await.unwrap_err();
        assert!(matches!(err, PluginError::SubprocessProtocol(_)), "{err:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_binary_surfaces_subprocess_missing() {
        let host = PluginHost::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manifest.toml"), MANIFEST_TEMPLATE).unwrap();
        // Note: no plugin.sh written.
        let staged = load(&host, &host_version(), dir.path()).unwrap();
        let err = activate(&host, "1.0.0-beta", &staged).await.unwrap_err();
        assert!(matches!(err, PluginError::SubprocessMissing(_)), "{err:?}");
    }
}
