//! `plamenix pack` — assemble a `.plx` from a directory that already
//! has its compiled artifacts in place (I7.13).
//!
//! Distinct from `plamenix build` (I7.12): `pack` does NOT compile
//! anything. It is the "bytes-in, archive-out" pure step. Useful
//! when a CI pipeline split building (Rust toolchain image, Node
//! toolchain image) from packaging (small image, plamenix-cli only),
//! or when an author has produced the artifacts manually + wants
//! the archive without running the full build orchestration.

use std::path::{Path, PathBuf};

use plamenix_plugin_host::{Manifest, write_plx};
use thiserror::Error;

/// Errors emitted by [`pack_plugin`].
#[derive(Debug, Error)]
pub enum PackError {
    #[error("plugin directory does not exist: {0}")]
    MissingPluginDir(PathBuf),
    #[error("manifest.toml missing in plugin directory: {0}")]
    MissingManifest(PathBuf),
    #[error("manifest parse failed: {0}")]
    ManifestParse(#[from] plamenix_plugin_host::PluginError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Output of a successful pack.
#[derive(Debug)]
pub struct PackOutput {
    pub plx_path: PathBuf,
    pub plugin_id: String,
    pub plugin_version: String,
}

/// Pack `plugin_dir` into a `.plx` at `output_dir/<id>-<version>.plx`.
/// `output_dir` defaults to `plugin_dir` itself when `None`.
///
/// # Errors
///
/// See [`PackError`].
pub fn pack_plugin(
    plugin_dir: &Path,
    output_dir: Option<&Path>,
) -> Result<PackOutput, PackError> {
    if !plugin_dir.is_dir() {
        return Err(PackError::MissingPluginDir(plugin_dir.to_path_buf()));
    }
    let manifest_path = plugin_dir.join("manifest.toml");
    if !manifest_path.is_file() {
        return Err(PackError::MissingManifest(manifest_path));
    }
    let manifest = Manifest::parse(&std::fs::read_to_string(&manifest_path)?)?;
    let output = output_dir.unwrap_or(plugin_dir);
    std::fs::create_dir_all(output)?;
    let archive_name = format!("{}-{}.plx", manifest.plugin.id, manifest.plugin.version);
    let plx_path = output.join(archive_name);
    write_plx(plugin_dir, &plx_path)?;
    Ok(PackOutput {
        plx_path,
        plugin_id: manifest.plugin.id,
        plugin_version: manifest.plugin.version.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn manifest_text(id: &str, version: &str) -> String {
        format!(
            r#"
[plugin]
id = "{id}"
name = "Pack Test"
version = "{version}"
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
"#,
        )
    }

    #[test]
    fn pack_produces_archive_named_after_id_and_version() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(plug.join("manifest.toml"), manifest_text("a.b.c", "1.2.3")).unwrap();
        fs::write(plug.join("ui.mjs"), "// ui").unwrap();
        let out = pack_plugin(&plug, None).unwrap();
        assert!(out.plx_path.is_file());
        assert!(out.plx_path.ends_with("a.b.c-1.2.3.plx"));
        assert_eq!(out.plugin_id, "a.b.c");
        assert_eq!(out.plugin_version, "1.2.3");
    }

    #[test]
    fn pack_uses_supplied_output_dir() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        let target = dir.path().join("artifacts");
        fs::create_dir(&plug).unwrap();
        fs::write(plug.join("manifest.toml"), manifest_text("z.z", "0.1.0")).unwrap();
        fs::write(plug.join("ui.mjs"), "// ui").unwrap();
        let out = pack_plugin(&plug, Some(&target)).unwrap();
        assert!(out.plx_path.starts_with(&target));
    }

    #[test]
    fn pack_errors_on_missing_dir() {
        let dir = tempdir().unwrap();
        let nope = dir.path().join("nope");
        let err = pack_plugin(&nope, None).unwrap_err();
        assert!(matches!(err, PackError::MissingPluginDir(_)));
    }

    #[test]
    fn pack_errors_on_missing_manifest() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        let err = pack_plugin(&plug, None).unwrap_err();
        assert!(matches!(err, PackError::MissingManifest(_)));
    }

    #[test]
    fn produced_plx_roundtrips() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(plug.join("manifest.toml"), manifest_text("rt.test", "5.6.7")).unwrap();
        fs::write(plug.join("ui.mjs"), "// ui").unwrap();
        let out = pack_plugin(&plug, None).unwrap();
        let m = plamenix_plugin_host::peek_plx_manifest(&out.plx_path).unwrap();
        assert_eq!(m.plugin.id, "rt.test");
        assert_eq!(m.plugin.version.to_string(), "5.6.7");
    }
}
