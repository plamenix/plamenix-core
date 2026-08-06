//! `plamenix build` — compile a plugin source tree into a `.plx`
//! archive (I7.12).
//!
//! Pipeline:
//!
//!   1. Parse `manifest.toml` (via `plamenix-plugin-host::Manifest`)
//!      to extract the plugin id + version + entry-point names.
//!   2. If `Cargo.toml` is present + the manifest references a wasm
//!      entry: run `cargo build --target wasm32-wasip2 --release`,
//!      then locate the resulting `.wasm` under `target/` and copy
//!      it next to the manifest as `plugin.wasm` (matching the
//!      manifest's expected `entry_points.wasm`).
//!   3. If `package.json` is present + the manifest references a UI
//!      entry: detect the JS package manager (pnpm > npm based on
//!      lockfile), install dependencies, run `<pm> run build`, copy
//!      the produced `dist/ui.mjs` (or top-level `ui.mjs`) next to
//!      the manifest as `ui.mjs`.
//!   4. Stage the plugin tree into a temp build dir + call
//!      `plamenix-plugin-host::write_plx` to produce
//!      `<plugin_id>-<version>.plx` in the requested output
//!      directory.
//!
//! Each step is independent; missing `Cargo.toml` or `package.json`
//! skips its half + emits an info line. A plugin can ship UI-only,
//! wasm-only, or both.

use std::path::{Path, PathBuf};
use std::process::Command;

use plamenix_plugin_host::{Manifest, write_plx};
use thiserror::Error;

/// Errors emitted by the build pipeline.
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("plugin directory does not exist: {0}")]
    MissingPluginDir(PathBuf),
    #[error("manifest.toml missing in plugin directory: {0}")]
    MissingManifest(PathBuf),
    #[error("manifest parse failed: {0}")]
    ManifestParse(#[from] plamenix_plugin_host::PluginError),
    #[error("cargo build failed (exit {code}): {stderr}")]
    CargoFailed { code: i32, stderr: String },
    #[error("could not locate the wasm artifact under {0}")]
    WasmNotFound(PathBuf),
    #[error("npm/pnpm install failed (exit {code}): {stderr}")]
    NpmInstallFailed { code: i32, stderr: String },
    #[error("npm/pnpm build failed (exit {code}): {stderr}")]
    NpmBuildFailed { code: i32, stderr: String },
    #[error("ui bundle not produced (looked at {0} and {1})")]
    UiBundleMissing(PathBuf, PathBuf),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Output of a successful build.
#[derive(Debug)]
pub struct BuildOutput {
    /// Path to the produced `.plx` archive.
    pub plx_path: PathBuf,
    /// Whether the wasm half ran.
    pub built_wasm: bool,
    /// Whether the UI half ran.
    pub built_ui: bool,
}

/// Options for [`build_plugin`].
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// Directory to drop the produced `.plx` into. Defaults to the
    /// plugin directory itself.
    pub output_dir: Option<PathBuf>,
    /// Skip running `cargo build` even when `Cargo.toml` is present.
    /// Used when the caller has already produced `plugin.wasm`
    /// in-tree (CI cache, manual override, etc.).
    pub skip_wasm: bool,
    /// Skip the UI build even when `package.json` is present. Same
    /// rationale as `skip_wasm`.
    pub skip_ui: bool,
    /// Skip `npm/pnpm install`. Useful in CI where the install
    /// already happened in a previous step.
    pub skip_install: bool,
}

/// Build a plugin source tree at `plugin_dir`. Returns the produced
/// `.plx` path + flags indicating which halves were built.
///
/// # Errors
///
/// See [`BuildError`].
pub fn build_plugin(plugin_dir: &Path, options: &BuildOptions) -> Result<BuildOutput, BuildError> {
    if !plugin_dir.is_dir() {
        return Err(BuildError::MissingPluginDir(plugin_dir.to_path_buf()));
    }
    let manifest_path = plugin_dir.join("manifest.toml");
    if !manifest_path.is_file() {
        return Err(BuildError::MissingManifest(manifest_path));
    }
    let manifest_text = std::fs::read_to_string(&manifest_path)?;
    let manifest = Manifest::parse(&manifest_text)?;

    let mut built_wasm = false;
    let cargo_toml = plugin_dir.join("Cargo.toml");
    let wasm_entry = manifest
        .entry_points
        .wasm
        .as_deref()
        .and_then(Path::to_str)
        .map(str::to_string);
    if wasm_entry.is_some() && cargo_toml.is_file() && !options.skip_wasm {
        let entry = wasm_entry.as_deref().unwrap_or("plugin.wasm");
        run_cargo_build(plugin_dir)?;
        copy_cargo_wasm(plugin_dir, entry)?;
        built_wasm = true;
    }

    let mut built_ui = false;
    let pkg_json = plugin_dir.join("package.json");
    let ui_entry = manifest
        .entry_points
        .ui
        .as_deref()
        .and_then(Path::to_str)
        .map(str::to_string);
    if ui_entry.is_some() && pkg_json.is_file() && !options.skip_ui {
        let entry = ui_entry.as_deref().unwrap_or("ui.mjs");
        let pm = detect_package_manager(plugin_dir);
        if !options.skip_install {
            run_pm_install(plugin_dir, pm)?;
        }
        run_pm_build(plugin_dir, pm)?;
        copy_ui_bundle(plugin_dir, entry)?;
        built_ui = true;
    }

    let output_dir = options
        .output_dir
        .clone()
        .unwrap_or_else(|| plugin_dir.to_path_buf());
    std::fs::create_dir_all(&output_dir)?;
    let archive_name = format!("{}-{}.plx", manifest.plugin.id, manifest.plugin.version,);
    let plx_path = output_dir.join(archive_name);
    write_plx(plugin_dir, &plx_path)?;

    Ok(BuildOutput {
        plx_path,
        built_wasm,
        built_ui,
    })
}

/// Package manager preference resolved by [`detect_package_manager`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    /// `pnpm` — picked when `pnpm-lock.yaml` is present.
    Pnpm,
    /// `npm` — fallback.
    Npm,
}

impl PackageManager {
    fn binary(self) -> &'static str {
        match self {
            Self::Pnpm => "pnpm",
            Self::Npm => "npm",
        }
    }
}

/// Detects the package manager based on lockfile presence. Returns
/// [`PackageManager::Pnpm`] when `pnpm-lock.yaml` exists in
/// `plugin_dir`, [`PackageManager::Npm`] otherwise.
pub fn detect_package_manager(plugin_dir: &Path) -> PackageManager {
    if plugin_dir.join("pnpm-lock.yaml").is_file() {
        PackageManager::Pnpm
    } else {
        PackageManager::Npm
    }
}

fn run_cargo_build(plugin_dir: &Path) -> Result<(), BuildError> {
    let output = Command::new("cargo")
        .current_dir(plugin_dir)
        .args(["build", "--target", "wasm32-wasip2", "--release"])
        .output()?;
    if !output.status.success() {
        return Err(BuildError::CargoFailed {
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    Ok(())
}

fn copy_cargo_wasm(plugin_dir: &Path, target_name: &str) -> Result<(), BuildError> {
    let target_dir = plugin_dir.join("target/wasm32-wasip2/release");
    let mut found: Option<PathBuf> = None;
    if target_dir.is_dir() {
        for entry in std::fs::read_dir(&target_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("wasm") {
                found = Some(path);
                break;
            }
        }
    }
    let src = found.ok_or_else(|| BuildError::WasmNotFound(target_dir))?;
    let dest = plugin_dir.join(target_name);
    std::fs::copy(&src, &dest)?;
    Ok(())
}

fn run_pm_install(plugin_dir: &Path, pm: PackageManager) -> Result<(), BuildError> {
    let output = Command::new(pm.binary())
        .current_dir(plugin_dir)
        .arg("install")
        .output()?;
    if !output.status.success() {
        return Err(BuildError::NpmInstallFailed {
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    Ok(())
}

fn run_pm_build(plugin_dir: &Path, pm: PackageManager) -> Result<(), BuildError> {
    let output = Command::new(pm.binary())
        .current_dir(plugin_dir)
        .args(["run", "build"])
        .output()?;
    if !output.status.success() {
        return Err(BuildError::NpmBuildFailed {
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    Ok(())
}

fn copy_ui_bundle(plugin_dir: &Path, target_name: &str) -> Result<(), BuildError> {
    let dest = plugin_dir.join(target_name);
    let dist_candidate = plugin_dir.join("dist").join(target_name);
    let top_candidate = plugin_dir.join(target_name);
    if dest == top_candidate && top_candidate.is_file() {
        // Already in place (vite output configured to root).
        return Ok(());
    }
    if dist_candidate.is_file() {
        std::fs::copy(&dist_candidate, &dest)?;
        return Ok(());
    }
    if top_candidate.is_file() {
        // The vite config left the bundle at the top level. Nothing
        // to do; dest == top_candidate handled above.
        return Ok(());
    }
    Err(BuildError::UiBundleMissing(dist_candidate, top_candidate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn manifest_text() -> &'static str {
        r#"
[plugin]
id = "test.build.fixture"
name = "Build Fixture"
version = "0.1.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"
ui = "ui.mjs"

[runtime]
restart_policy = "transient"

[permissions]
required = []
optional = []
"#
    }

    fn ui_only_manifest_text() -> &'static str {
        r#"
[plugin]
id = "test.build.ui"
name = "UI Only"
version = "0.2.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"

[runtime]
restart_policy = "transient"

[permissions]
required = []
optional = []
"#
    }

    #[test]
    fn build_plugin_packs_pre_built_artifacts_when_skip_flags_set() {
        // Pre-stage wasm + ui then build with skip flags so no
        // the child process fires. The test exercises the orchestration +
        // archive path, not the cargo/npm side.
        let dir = tempdir().unwrap();
        let plug = dir.path().join("plugin");
        fs::create_dir(&plug).unwrap();
        fs::write(plug.join("manifest.toml"), manifest_text()).unwrap();
        fs::write(plug.join("plugin.wasm"), b"\x00asm\x01\x00\x00\x00").unwrap();
        fs::write(plug.join("ui.mjs"), "// ui").unwrap();

        let output = build_plugin(
            &plug,
            &BuildOptions {
                skip_wasm: true,
                skip_ui: true,
                ..BuildOptions::default()
            },
        )
        .unwrap();
        assert!(output.plx_path.is_file());
        assert!(output.plx_path.ends_with("test.build.fixture-0.1.0.plx"));
        assert!(!output.built_wasm);
        assert!(!output.built_ui);
    }

    #[test]
    fn build_plugin_uses_output_dir_when_supplied() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(plug.join("manifest.toml"), ui_only_manifest_text()).unwrap();
        fs::write(plug.join("ui.mjs"), "// ui").unwrap();

        let outdir = dir.path().join("artifacts");
        let output = build_plugin(
            &plug,
            &BuildOptions {
                output_dir: Some(outdir.clone()),
                skip_ui: true,
                ..BuildOptions::default()
            },
        )
        .unwrap();
        assert!(output.plx_path.starts_with(&outdir));
        assert!(output.plx_path.ends_with("test.build.ui-0.2.0.plx"));
    }

    #[test]
    fn build_plugin_errors_on_missing_dir() {
        let dir = tempdir().unwrap();
        let nope = dir.path().join("no-such");
        let err = build_plugin(&nope, &BuildOptions::default()).unwrap_err();
        assert!(matches!(err, BuildError::MissingPluginDir(_)));
    }

    #[test]
    fn build_plugin_errors_on_missing_manifest() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        let err = build_plugin(&plug, &BuildOptions::default()).unwrap_err();
        assert!(matches!(err, BuildError::MissingManifest(_)));
    }

    #[test]
    fn build_plugin_skips_wasm_when_manifest_has_no_wasm_entry() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(plug.join("manifest.toml"), ui_only_manifest_text()).unwrap();
        // Pre-stage ui.mjs so the packer accepts the bundle. Cargo
        // not invoked because there's no wasm entry in the manifest.
        fs::write(plug.join("ui.mjs"), "// ui").unwrap();
        let output = build_plugin(
            &plug,
            &BuildOptions {
                skip_ui: true,
                ..BuildOptions::default()
            },
        )
        .unwrap();
        assert!(!output.built_wasm);
        assert!(!output.built_ui);
        assert!(output.plx_path.is_file());
    }

    #[test]
    fn build_plugin_skips_ui_when_manifest_has_no_ui_entry() {
        let wasm_only = r#"
[plugin]
id = "test.build.wasm"
name = "Wasm Only"
version = "0.3.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"

[runtime]
restart_policy = "transient"

[permissions]
required = []
optional = []
"#;
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(plug.join("manifest.toml"), wasm_only).unwrap();
        fs::write(plug.join("plugin.wasm"), b"\x00asm\x01\x00\x00\x00").unwrap();
        let output = build_plugin(
            &plug,
            &BuildOptions {
                skip_wasm: true,
                ..BuildOptions::default()
            },
        )
        .unwrap();
        assert!(!output.built_wasm);
        assert!(!output.built_ui);
        assert!(output.plx_path.is_file());
    }

    #[test]
    fn detect_package_manager_picks_pnpm_when_lockfile_present() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("pnpm-lock.yaml"), "lockfile").unwrap();
        assert_eq!(detect_package_manager(dir.path()), PackageManager::Pnpm);
    }

    #[test]
    fn detect_package_manager_defaults_to_npm() {
        let dir = tempdir().unwrap();
        assert_eq!(detect_package_manager(dir.path()), PackageManager::Npm);
    }

    #[test]
    fn produced_plx_roundtrips_through_peek_plx_manifest() {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(plug.join("manifest.toml"), manifest_text()).unwrap();
        fs::write(plug.join("plugin.wasm"), b"\x00asm\x01\x00\x00\x00").unwrap();
        fs::write(plug.join("ui.mjs"), "// ui").unwrap();
        let output = build_plugin(
            &plug,
            &BuildOptions {
                skip_wasm: true,
                skip_ui: true,
                ..BuildOptions::default()
            },
        )
        .unwrap();
        let m = plamenix_plugin_host::peek_plx_manifest(&output.plx_path).unwrap();
        assert_eq!(m.plugin.id, "test.build.fixture");
        assert_eq!(m.plugin.version.to_string(), "0.1.0");
    }
}
