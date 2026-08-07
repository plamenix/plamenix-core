//! Plugin bundle loader.
//!
//! Reads a plugin bundle directory (containing `manifest.toml` plus the
//! files referenced by `entry_points`), validates the manifest against
//! the host version, and stages the wasm component in the wasmtime
//! engine. Phase B will add `activate()` invocation; for now this stops
//! at component compilation so the host can prove a plugin is loadable
//! without committing to a permissions handshake.

use std::path::{Path, PathBuf};

use semver::Version;
use wasmtime::component::Component;

use crate::error::PluginError;
use crate::host::PluginHost;
use crate::manifest::Manifest;

const MANIFEST_FILENAME: &str = "manifest.toml";

/// A staged plugin: its parsed manifest plus the compiled wasmtime
/// [`Component`] when a wasm half is present. The component is
/// instantiation-ready; the loader does not call any of its exports.
pub struct StagedPlugin {
    /// Filesystem path to the plugin bundle root.
    pub bundle_dir: PathBuf,
    /// Parsed `manifest.toml`.
    pub manifest: Manifest,
    /// Compiled `wasm` component, if the plugin ships a wasm half.
    pub component: Option<Component>,
}

impl std::fmt::Debug for StagedPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedPlugin")
            .field("bundle_dir", &self.bundle_dir)
            .field("manifest", &self.manifest)
            .field("component", &self.component.as_ref().map(|_| "<Component>"))
            .finish()
    }
}

/// Loads and stages the plugin bundle at `bundle_dir`.
///
/// Steps:
///
/// 1. Read and parse `manifest.toml`.
/// 2. Verify the host's `SemVer` is in the plugin's
///    `plamenix_min_version` range.
/// 3. If a wasm entry point is present, compile the `.wasm` file
///    against the host's wasmtime engine.
///
/// The returned [`StagedPlugin`] is ready for the Phase B activation
/// pipeline (instantiate + call `activate`). The loader never runs
/// plugin code.
///
/// # Errors
///
/// Returns [`PluginError::ManifestMissing`] if `manifest.toml` is
/// absent, [`PluginError::InvalidManifest`] / `InvalidCapability` if
/// the manifest is malformed, [`PluginError::IncompatibleHost`] if the
/// host version is below the plugin's required range, or
/// [`PluginError::WasmMissing`] / `Runtime` if the wasm file is missing
/// or fails to compile.
#[tracing::instrument(
    name = "plugin.load",
    skip(host, host_version, bundle_dir),
    fields(bundle = %bundle_dir.as_ref().display()),
)]
pub fn load(
    host: &PluginHost,
    host_version: &Version,
    bundle_dir: impl AsRef<Path>,
) -> Result<StagedPlugin, PluginError> {
    let bundle_dir = bundle_dir.as_ref().to_path_buf();
    let manifest_path = bundle_dir.join(MANIFEST_FILENAME);

    let manifest_text = std::fs::read_to_string(&manifest_path)
        .map_err(|_| PluginError::ManifestMissing(manifest_path.clone()))?;
    let manifest = Manifest::parse(&manifest_text)?;

    if !manifest.supports_host(host_version) {
        return Err(PluginError::IncompatibleHost {
            host: host_version.to_string(),
            required: manifest.plugin.plamenix_min_version.to_string(),
        });
    }

    let component = match manifest.entry_points.wasm.as_ref() {
        Some(rel) => {
            // Only wasm halves get linked, so only they care which
            // world was declared. A UI-only plugin may name any world
            // in the contract without the host having to satisfy it.
            crate::world::check_linkable(manifest.plugin.world_tier)?;
            let wasm_path = bundle_dir.join(rel);
            if !wasm_path.exists() {
                return Err(PluginError::WasmMissing(wasm_path));
            }
            let component = Component::from_file(host.engine(), &wasm_path)
                .map_err(|err| PluginError::Runtime(err.to_string()))?;
            Some(component)
        }
        None => None,
    };

    tracing::info!(
        id = %manifest.plugin.id,
        version = %manifest.plugin.version,
        "plugin staged",
    );

    Ok(StagedPlugin {
        bundle_dir,
        manifest,
        component,
    })
}
