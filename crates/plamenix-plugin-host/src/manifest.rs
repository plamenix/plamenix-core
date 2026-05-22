//! `manifest.toml` parser.
//!
//! Mirrors the schema in `../../../docs/plugin-manifest.md`. Parsing
//! happens in two stages: a thin serde mirror, then a validating
//! conversion to the strongly-typed [`Manifest`] used by the rest of
//! the host. The split keeps deserialisation forgiving (unknown keys
//! flagged as warnings, not errors) while the typed form rejects
//! anything that would unsafely reach the runtime.

use std::path::PathBuf;

use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::capability::{Permission, PermissionSet};
use crate::error::PluginError;

const RUNTIME_SUBPROCESS_CAPABILITY: Permission = Permission::RuntimeSubprocess;

/// A parsed and validated plugin manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// Identity and version metadata.
    pub plugin: PluginMetadata,
    /// Required and optional capabilities the plugin requests.
    pub permissions: PermissionSet,
    /// File paths within the bundle for wasm and UI halves.
    pub entry_points: EntryPoints,
    /// Runtime-mode flags (subprocess, sandbox opt-outs).
    pub runtime: RuntimeFlags,
    /// Declarative UI contributions advertised by the plugin. Optional —
    /// plugins that only do background work leave this empty.
    #[serde(default)]
    pub contributions: Contributions,
}

/// Declarative contribution points. Plugins ship these as static
/// manifest entries so the host can render them without running the
/// plugin first; the plugin's runtime half augments them with state.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Contributions {
    /// Side-panel contributions surfaced by the host's sidebar shell.
    #[serde(default)]
    pub sidebar_panels: Vec<SidebarPanel>,
}

/// One sidebar-panel contribution.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SidebarPanel {
    /// Stable identifier; used by the host to route clicks to the
    /// plugin's UI half (when it ships one).
    pub id: String,
    /// Human-readable label shown in the sidebar.
    pub label: String,
    /// Optional Lucide-react icon name. The host resolves the string
    /// to a concrete component; unknown icons fall back to a default.
    pub icon: Option<String>,
}

/// `[plugin]` table — identity and host-compatibility metadata.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginMetadata {
    /// Reverse-DNS identifier; unique across the ecosystem.
    pub id: String,
    /// Human-readable name shown in the plugin manager.
    pub name: String,
    /// Plugin's own `SemVer`.
    pub version: Version,
    /// Host `SemVer` range the plugin requires.
    pub plamenix_min_version: VersionReq,
    /// `WIT` package version the plugin targets (e.g. "1.0").
    pub plugin_api: String,
    /// Optional author line (`Name <email>`).
    pub author: Option<String>,
    /// Optional `SPDX` licence string.
    pub license: Option<String>,
    /// Optional homepage URL.
    pub homepage: Option<String>,
    /// Optional short description.
    pub description: Option<String>,
}

/// `[entry_points]` table — bundle-relative paths to plugin halves.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EntryPoints {
    /// Path to the WASM component (relative to bundle root). Optional;
    /// some plugins ship only a UI half.
    pub wasm: Option<PathBuf>,
    /// Path to the ESM module exporting React contributions. Optional;
    /// some plugins ship only a backend half.
    pub ui: Option<PathBuf>,
    /// Path to a native executable for plugins that opt out of the
    /// WASM sandbox via `runtime.requires_subprocess`. Required when
    /// that flag is set, ignored otherwise.
    pub subprocess: Option<PathBuf>,
}

/// `[runtime]` table — flags that change how the host loads the plugin.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeFlags {
    /// When `true`, the plugin must run in a subprocess (out of the
    /// WASM sandbox). Requires `runtime.subprocess` capability.
    pub requires_subprocess: bool,
}

impl Manifest {
    /// Parses the given TOML text into a validated [`Manifest`].
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::InvalidManifest`] when the input is not
    /// valid TOML or fails schema validation (missing required fields,
    /// neither entry point present, etc.), or
    /// [`PluginError::InvalidCapability`] when a permission string
    /// fails to parse.
    pub fn parse(text: &str) -> Result<Self, PluginError> {
        let raw: RawManifest =
            toml::from_str(text).map_err(|err| PluginError::InvalidManifest(err.to_string()))?;
        raw.try_into()
    }

    /// Returns `true` when the plugin satisfies the supplied host `SemVer`.
    #[must_use]
    pub fn supports_host(&self, host_version: &Version) -> bool {
        self.plugin.plamenix_min_version.matches(host_version)
    }
}

/// Untyped serde mirror used as the parse intermediate.
#[derive(Debug, Deserialize)]
struct RawManifest {
    plugin: RawPlugin,
    #[serde(default)]
    permissions: RawPermissions,
    entry_points: RawEntryPoints,
    #[serde(default)]
    runtime: RuntimeFlags,
    #[serde(default)]
    contributions: Contributions,
}

#[derive(Debug, Deserialize)]
struct RawPlugin {
    id: String,
    name: String,
    version: String,
    plamenix_min_version: String,
    plugin_api: String,
    author: Option<String>,
    license: Option<String>,
    homepage: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPermissions {
    #[serde(default)]
    required: Vec<String>,
    #[serde(default)]
    optional: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawEntryPoints {
    wasm: Option<PathBuf>,
    ui: Option<PathBuf>,
    subprocess: Option<PathBuf>,
}

impl TryFrom<RawManifest> for Manifest {
    type Error = PluginError;

    fn try_from(raw: RawManifest) -> Result<Self, Self::Error> {
        if raw.entry_points.wasm.is_none()
            && raw.entry_points.ui.is_none()
            && raw.entry_points.subprocess.is_none()
        {
            return Err(PluginError::InvalidManifest(
                "entry_points must define at least one of `wasm`, `ui`, or `subprocess`".into(),
            ));
        }

        if raw.runtime.requires_subprocess && raw.entry_points.subprocess.is_none() {
            return Err(PluginError::MissingSubprocessEntry);
        }

        let plugin = PluginMetadata {
            id: raw.plugin.id,
            name: raw.plugin.name,
            version: raw
                .plugin
                .version
                .parse()
                .map_err(|err: semver::Error| PluginError::InvalidManifest(err.to_string()))?,
            plamenix_min_version: raw
                .plugin
                .plamenix_min_version
                .parse()
                .map_err(|err: semver::Error| PluginError::InvalidManifest(err.to_string()))?,
            plugin_api: raw.plugin.plugin_api,
            author: raw.plugin.author,
            license: raw.plugin.license,
            homepage: raw.plugin.homepage,
            description: raw.plugin.description,
        };

        let required = raw
            .permissions
            .required
            .into_iter()
            .map(|raw| Permission::parse(&raw))
            .collect::<Result<Vec<_>, _>>()?;
        let optional = raw
            .permissions
            .optional
            .into_iter()
            .map(|raw| Permission::parse(&raw))
            .collect::<Result<Vec<_>, _>>()?;

        let permissions = PermissionSet { required, optional };

        if raw.runtime.requires_subprocess && !permissions.grants(&RUNTIME_SUBPROCESS_CAPABILITY) {
            return Err(PluginError::MissingSubprocessCapability);
        }

        Ok(Self {
            plugin,
            permissions,
            entry_points: EntryPoints {
                wasm: raw.entry_points.wasm,
                ui: raw.entry_points.ui,
                subprocess: raw.entry_points.subprocess,
            },
            runtime: raw.runtime,
            contributions: raw.contributions,
        })
    }
}
