//! `plamenix validate` — manifest-only validation (I7.13).
//!
//! Accepts either a `.plx` archive OR a plugin source directory.
//! Reads the manifest (via `peek_plx_manifest` for archives or the
//! filesystem for directories), runs it through the host's
//! `Manifest::parse` schema check, and returns a structured
//! [`ValidationReport`] the CLI prints as a human-readable summary.
//!
//! Use cases:
//!
//! - Pre-flight check in CI before invoking `plamenix build`.
//! - Bundle audit step before uploading via `POST /api/plugins/install`.
//! - Author sanity-check after editing `manifest.toml`.

use std::path::{Path, PathBuf};

use plamenix_plugin_host::{Manifest, peek_plx_manifest};
use thiserror::Error;

/// Errors emitted by [`validate_target`].
#[derive(Debug, Error)]
pub enum ValidateError {
    #[error("target does not exist: {0}")]
    Missing(PathBuf),
    #[error("manifest.toml missing in directory: {0}")]
    DirectoryMissingManifest(PathBuf),
    #[error("manifest parse / schema check failed: {0}")]
    ManifestParse(#[from] plamenix_plugin_host::PluginError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Summary returned to the CLI on a successful validate.
#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub plugin_id: String,
    pub plugin_name: String,
    pub plugin_version: String,
    pub plugin_api: String,
    /// Number of required permissions declared by the manifest.
    pub required_count: usize,
    /// Number of optional permissions declared by the manifest.
    pub optional_count: usize,
    /// Was the wasm entry point declared?
    pub has_wasm_entry: bool,
    /// Was the UI entry point declared?
    pub has_ui_entry: bool,
    /// Was the subprocess entry point declared?
    pub has_subprocess_entry: bool,
    /// Did the manifest set `runtime.requires_subprocess`?
    pub requires_subprocess: bool,
}

impl ValidationReport {
    fn from_manifest(manifest: &Manifest) -> Self {
        Self {
            plugin_id: manifest.plugin.id.clone(),
            plugin_name: manifest.plugin.name.clone(),
            plugin_version: manifest.plugin.version.to_string(),
            plugin_api: manifest.plugin.plugin_api.clone(),
            required_count: manifest.permissions.required.len(),
            optional_count: manifest.permissions.optional.len(),
            has_wasm_entry: manifest.entry_points.wasm.is_some(),
            has_ui_entry: manifest.entry_points.ui.is_some(),
            has_subprocess_entry: manifest.entry_points.subprocess.is_some(),
            requires_subprocess: manifest.runtime.requires_subprocess,
        }
    }
}

/// Validates either a `.plx` archive (when `target` ends in `.plx`
/// or is a regular file) or a plugin source directory (when `target`
/// is a directory).
///
/// # Errors
///
/// See [`ValidateError`].
pub fn validate_target(target: &Path) -> Result<ValidationReport, ValidateError> {
    if !target.exists() {
        return Err(ValidateError::Missing(target.to_path_buf()));
    }
    let manifest = if target.is_dir() {
        let manifest_path = target.join("manifest.toml");
        if !manifest_path.is_file() {
            return Err(ValidateError::DirectoryMissingManifest(
                target.to_path_buf(),
            ));
        }
        Manifest::parse(&std::fs::read_to_string(&manifest_path)?)?
    } else {
        peek_plx_manifest(target)?
    };
    Ok(ValidationReport::from_manifest(&manifest))
}

/// Formats a [`ValidationReport`] for human display.
#[must_use]
pub fn render_report(report: &ValidationReport) -> String {
    let entries = [
        report.has_wasm_entry.then_some("wasm"),
        report.has_ui_entry.then_some("ui"),
        report.has_subprocess_entry.then_some("subprocess"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
    let entries_display = if entries.is_empty() {
        "(none)".to_string()
    } else {
        entries
    };
    format!(
        "{name} ({id}) v{version}\n  api:       {api}\n  entries:   {entries}\n  perms:     {req} required, {opt} optional\n  subproc:   {sub}",
        name = report.plugin_name,
        id = report.plugin_id,
        version = report.plugin_version,
        api = report.plugin_api,
        entries = entries_display,
        req = report.required_count,
        opt = report.optional_count,
        sub = if report.requires_subprocess {
            "yes"
        } else {
            "no"
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use plamenix_plugin_host::write_plx;
    use std::fs;
    use tempfile::tempdir;

    fn manifest_text(id: &str, version: &str, required: &[&str], optional: &[&str]) -> String {
        let req_list = required
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let opt_list = optional
            .iter()
            .map(|p| format!("\"{p}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"
[plugin]
id = "{id}"
name = "Validate Test"
version = "{version}"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"

[runtime]
restart_policy = "transient"
requires_subprocess = false

[permissions]
required = [{req_list}]
optional = [{opt_list}]
"#,
        )
    }

    #[test]
    fn validate_directory_returns_report() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(
            plug.join("manifest.toml"),
            manifest_text("a.b", "0.1.0", &[], &[]),
        )
        .unwrap();
        let report = validate_target(&plug).unwrap();
        assert_eq!(report.plugin_id, "a.b");
        assert_eq!(report.plugin_version, "0.1.0");
        assert!(report.has_ui_entry);
        assert!(!report.has_wasm_entry);
        assert_eq!(report.required_count, 0);
        assert_eq!(report.optional_count, 0);
    }

    #[test]
    fn validate_directory_counts_permissions() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(
            plug.join("manifest.toml"),
            manifest_text(
                "c.d",
                "0.1.0",
                &["db.read.any"],
                &["db.schema.list", "net.https"],
            ),
        )
        .unwrap();
        let report = validate_target(&plug).unwrap();
        assert_eq!(report.required_count, 1);
        assert_eq!(report.optional_count, 2);
    }

    #[test]
    fn validate_archive_returns_report() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(
            plug.join("manifest.toml"),
            manifest_text("arc.v", "9.9.9", &[], &[]),
        )
        .unwrap();
        fs::write(plug.join("ui.mjs"), "// ui").unwrap();
        let plx = dir.path().join("out.plx");
        write_plx(&plug, &plx).unwrap();
        let report = validate_target(&plx).unwrap();
        assert_eq!(report.plugin_id, "arc.v");
        assert_eq!(report.plugin_version, "9.9.9");
    }

    #[test]
    fn validate_errors_on_missing_target() {
        let dir = tempdir().unwrap();
        let nope = dir.path().join("nope");
        let err = validate_target(&nope).unwrap_err();
        assert!(matches!(err, ValidateError::Missing(_)));
    }

    #[test]
    fn validate_errors_on_directory_without_manifest() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        let err = validate_target(&plug).unwrap_err();
        assert!(matches!(err, ValidateError::DirectoryMissingManifest(_)));
    }

    #[test]
    fn render_report_includes_all_fields() {
        let report = ValidationReport {
            plugin_id: "x.y".to_string(),
            plugin_name: "X Y".to_string(),
            plugin_version: "1.0.0".to_string(),
            plugin_api: "1.0".to_string(),
            required_count: 1,
            optional_count: 2,
            has_wasm_entry: true,
            has_ui_entry: true,
            has_subprocess_entry: false,
            requires_subprocess: false,
        };
        let s = render_report(&report);
        assert!(s.contains("X Y (x.y) v1.0.0"));
        assert!(s.contains("wasm, ui"));
        assert!(s.contains("1 required, 2 optional"));
        assert!(s.contains("subproc:   no"));
    }
}
