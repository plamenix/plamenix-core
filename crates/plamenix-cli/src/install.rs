//! `plamenix install` — install a `.plx` into a running host (I7.13).
//!
//! ## M1 status: stub
//!
//! The install subcommand is **stubbed** for M1. The real
//! implementation needs:
//!
//!   - An HTTP client to POST the bundle to the web edition's
//!     `POST /api/plugins/install` endpoint (I7.8).
//!   - A base64 encoder for the JSON body.
//!   - A Tauri/IPC client for the desktop edition.
//!
//! Both deps are intentionally NOT added during the M1 freeze
//! window. The stub validates inputs + prints a clear next-action
//! message so authors using the CLI in CI scripts get a known exit
//! code instead of a "command not found" surprise.
//!
//! Land the live implementation in an M2 follow-up that adds:
//!   - `ureq = "2"` for the HTTP client (sync, no async runtime
//!     pulled in by `reqwest`).
//!   - `base64 = "0.22"` for the JSON body encoding.

use std::path::{Path, PathBuf};

use plamenix_plugin_host::peek_plx_manifest;
use thiserror::Error;

/// Errors emitted by [`install_plugin`].
#[derive(Debug, Error)]
pub enum InstallError {
    #[error("`.plx` archive missing: {0}")]
    ArchiveMissing(PathBuf),
    #[error("manifest validation failed: {0}")]
    ManifestParse(#[from] plamenix_plugin_host::PluginError),
    #[error(
        "install subcommand not yet implemented for M1 — POST to {0}/api/plugins/install with {{ bundleBase64, grantedOptional }} manually until the HTTP client lands"
    )]
    NotYetImplemented(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result of an install — present so the eventual non-stub
/// implementation can return a structured response (installed
/// plugin id + persisted grants) without breaking the CLI shape.
#[derive(Debug)]
pub struct InstallOutput {
    pub plugin_id: String,
    pub plugin_version: String,
    pub granted_optional: Vec<String>,
}

/// Install a `.plx` archive against the supplied `server` URL.
///
/// **M1 stub**: validates the archive exists + parses its manifest,
/// then returns [`InstallError::NotYetImplemented`] with a clear
/// next-action message. The live implementation lands in an M2
/// follow-up.
///
/// # Errors
///
/// See [`InstallError`].
pub fn install_plugin(
    archive: &Path,
    server: &str,
    _granted_optional: &[String],
) -> Result<InstallOutput, InstallError> {
    if !archive.is_file() {
        return Err(InstallError::ArchiveMissing(archive.to_path_buf()));
    }
    // Pre-flight manifest parse so the CLI catches malformed
    // archives before pretending to install. Real implementation
    // will reuse the parsed manifest to surface the install dialog's
    // info OR pass the bundle bytes to the server's
    // `plugin_validate_bundle` endpoint.
    let _ = peek_plx_manifest(archive)?;
    Err(InstallError::NotYetImplemented(server.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use plamenix_plugin_host::write_plx;
    use std::fs;
    use tempfile::tempdir;

    fn manifest_text() -> &'static str {
        r#"
[plugin]
id = "install.stub.test"
name = "Install Stub"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"

[runtime]
restart_policy = "transient"
requires_subprocess = false

[permissions]
required = []
optional = []
"#
    }

    #[test]
    fn install_returns_not_yet_implemented_on_valid_archive() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(plug.join("manifest.toml"), manifest_text()).unwrap();
        fs::write(plug.join("ui.mjs"), "// ui").unwrap();
        let plx = dir.path().join("test.plx");
        write_plx(&plug, &plx).unwrap();
        let err = install_plugin(&plx, "http://localhost:3000", &[]).unwrap_err();
        assert!(matches!(err, InstallError::NotYetImplemented(_)));
    }

    #[test]
    fn install_errors_on_missing_archive() {
        let dir = tempdir().unwrap();
        let nope = dir.path().join("nope.plx");
        let err = install_plugin(&nope, "http://localhost:3000", &[]).unwrap_err();
        assert!(matches!(err, InstallError::ArchiveMissing(_)));
    }

    #[test]
    fn install_errors_on_malformed_archive() {
        // A non-zip file should bubble up as a manifest-parse error
        // because peek_plx_manifest will fail to open the zip.
        let dir = tempdir().unwrap();
        let fake = dir.path().join("not-a-zip.plx");
        fs::write(&fake, b"not actually a zip").unwrap();
        let err = install_plugin(&fake, "http://localhost:3000", &[]).unwrap_err();
        assert!(matches!(err, InstallError::ManifestParse(_)));
    }
}
