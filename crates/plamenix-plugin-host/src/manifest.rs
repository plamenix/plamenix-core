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

use crate::capability::{Permission, PermissionGrant, PermissionSet};
use crate::error::PluginError;

const DEFAULT_WORLD: &str = "plamenix:plugin@1.0.0/plugin-minimal";

/// Plamenix edition a plugin can target.
///
/// Manifests declare a `targets` list; the host filters plugins whose
/// declared targets exclude the current edition at install time. Pure
/// WASM plugins using only universal capabilities should target both;
/// plugins reaching for the OS keyring should
/// declare desktop-only.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Edition {
    /// Tauri 2 desktop edition.
    Desktop,
    /// Fastify + React web edition.
    Web,
}

impl Edition {
    fn from_token(token: &str) -> Result<Self, PluginError> {
        match token {
            "desktop" => Ok(Self::Desktop),
            "web" => Ok(Self::Web),
            other => Err(PluginError::InvalidManifest(format!(
                "unknown target edition `{other}` (valid: \"desktop\", \"web\")"
            ))),
        }
    }

    /// Canonical string form used in manifests and CLI output.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Web => "web",
        }
    }
}

/// Supervisor restart policy. Mirrors OTP semantics (per
/// `PLUGIN_ARCHITECTURE.md` §12).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Always restart on any exit. Reserve for first-party essential
    /// plugins; the crash budget still applies.
    Permanent,
    /// Restart on abnormal exits only (trap, fuel exhaustion, etc.).
    /// Default for community plugins.
    #[default]
    Transient,
    /// Never restart. Use for one-shot tools and batch imports.
    Temporary,
}

impl RestartPolicy {
    fn from_token(token: &str) -> Result<Self, PluginError> {
        match token {
            "permanent" => Ok(Self::Permanent),
            "transient" => Ok(Self::Transient),
            "temporary" => Ok(Self::Temporary),
            other => Err(PluginError::InvalidManifest(format!(
                "unknown restart_policy `{other}` (valid: \"permanent\", \"transient\", \"temporary\")"
            ))),
        }
    }
}

/// A parsed and validated plugin manifest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// Identity and version metadata.
    pub plugin: PluginMetadata,
    /// Required and optional capabilities the plugin requests.
    pub permissions: PermissionSet,
    /// File paths within the bundle for wasm and UI halves.
    pub entry_points: EntryPoints,
    /// `[runtime]` flags — currently the optional resource limits.
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
    /// Event topics the plugin wants to receive (I6.2). Each entry is
    /// a topic pattern in the grammar documented at
    /// [`crate::event_bus`]: slash-separated namespace with `*`
    /// (single-segment wildcard) or `**` (zero-or-more trailing
    /// segments). The host validates patterns at manifest parse +
    /// registers each via `EventBus::subscribe(plugin_id, pattern)` at
    /// activation; subscriptions are dropped via
    /// `unsubscribe_by_plugin` at deactivation (or supervisor
    /// teardown). When the host dispatches a matching event it calls
    /// the plugin's exported `handle-event(topic, payload)` on each
    /// matched subscription.
    #[serde(default)]
    pub event_subscriptions: Vec<String>,
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
    /// Fully qualified WIT world identifier the plugin targets, in the
    /// form `<package>:<name>@<version>/<world>` — for example
    /// `plamenix:plugin@1.0.0/plugin-minimal`. The host links only the
    /// imports this world exposes; capabilities outside the world's
    /// surface are unreachable even if granted. Defaults to
    /// `plamenix:plugin@1.0.0/plugin-minimal`.
    pub world: String,
    /// Editions the plugin runs on. Host refuses to install when the
    /// list excludes the current edition. Defaults to `[Desktop, Web]`.
    pub targets: Vec<Edition>,
    /// Supervisor restart policy. Defaults to `Transient` — restart on
    /// abnormal exits, leave normal shutdowns alone.
    pub restart_policy: RestartPolicy,
    /// Optional author line (`Name <email>`).
    pub author: Option<String>,
    /// Optional `SPDX` licence string.
    pub license: Option<String>,
    /// Optional homepage URL.
    pub homepage: Option<String>,
    /// Optional short description.
    pub description: Option<String>,
}

impl PluginMetadata {
    /// Returns `true` when the plugin's `targets` list includes the
    /// supplied edition. Used at install time to refuse plugins that
    /// declare a target this host does not satisfy.
    #[must_use]
    pub fn supports_edition(&self, edition: Edition) -> bool {
        self.targets.contains(&edition)
    }
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
}

/// `[runtime]` table — flags that change how the host loads the plugin.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeFlags {
    /// I8.6 — optional `[runtime.limits]` table. Override per-store
    /// wasmtime caps (memory in MiB, table/instance counts). Missing
    /// fields leave the default in place. Hosts MAY refuse to grant
    /// requested overrides through the Permissions panel.
    #[serde(default)]
    pub limits: RuntimeLimits,
}

/// `[runtime.limits]` table — declarative per-store resource caps.
/// Missing fields fall back to the I8.6 defaults from
/// [`crate::limits`].
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct RuntimeLimits {
    /// Linear memory cap in MiB. `None` keeps the 64 MiB default.
    #[serde(default)]
    pub max_memory_mib: Option<usize>,
    /// Per-table element cap. `None` keeps the 10 000 default.
    #[serde(default)]
    pub max_table_elements: Option<usize>,
    /// Component instance cap. `None` keeps the 64 default.
    #[serde(default)]
    pub max_instances: Option<usize>,
    /// Table count cap. `None` keeps the 16 default.
    #[serde(default)]
    pub max_tables: Option<usize>,
    /// Memory count cap. `None` keeps the 4 default.
    #[serde(default)]
    pub max_memories: Option<usize>,
    /// I8.8 — concurrent in-flight call cap per plugin. `None`
    /// keeps the 4 default from
    /// [`crate::concurrency::MAX_IN_FLIGHT_DEFAULT`].
    #[serde(default)]
    pub max_in_flight: Option<usize>,
}

impl RuntimeLimits {
    /// Converts the manifest table into a
    /// [`crate::limits::ResourceLimitsOverride`].
    #[must_use]
    pub fn as_override(&self) -> crate::limits::ResourceLimitsOverride {
        crate::limits::ResourceLimitsOverride {
            max_memory_mib: self.max_memory_mib,
            max_table_elements: self.max_table_elements,
            max_instances: self.max_instances,
            max_tables: self.max_tables,
            max_memories: self.max_memories,
        }
    }

    /// Resolves the manifest override against the I8.6 defaults.
    #[must_use]
    pub fn resolve(&self) -> crate::limits::ResourceLimits {
        crate::limits::ResourceLimits::from_manifest_override(&self.as_override())
    }
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
    #[serde(default)]
    world: Option<String>,
    #[serde(default)]
    targets: Option<Vec<String>>,
    #[serde(default)]
    restart_policy: Option<String>,
    author: Option<String>,
    license: Option<String>,
    homepage: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawPermissions {
    #[serde(default)]
    required: Vec<RawPermissionGrant>,
    #[serde(default)]
    optional: Vec<RawPermissionGrant>,
}

/// Untagged serde wrapper that accepts either a bare capability
/// string OR an object with `capability` + `purpose`. Untagged so
/// both forms can sit in the same list, e.g. one legacy plain-string
/// entry alongside an object entry carrying a purpose.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawPermissionGrant {
    Plain(String),
    Detailed {
        capability: String,
        #[serde(default)]
        purpose: Option<String>,
    },
}

impl RawPermissionGrant {
    fn into_grant(self) -> Result<PermissionGrant, PluginError> {
        match self {
            Self::Plain(raw) => Ok(PermissionGrant::new(Permission::parse(&raw)?)),
            Self::Detailed {
                capability,
                purpose,
            } => {
                let cap = Permission::parse(&capability)?;
                Ok(match purpose {
                    Some(p) if !p.is_empty() => PermissionGrant::with_purpose(cap, p),
                    _ => PermissionGrant::new(cap),
                })
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawEntryPoints {
    wasm: Option<PathBuf>,
    ui: Option<PathBuf>,
}

impl TryFrom<RawManifest> for Manifest {
    type Error = PluginError;

    fn try_from(raw: RawManifest) -> Result<Self, Self::Error> {
        if raw.entry_points.wasm.is_none() && raw.entry_points.ui.is_none() {
            return Err(PluginError::InvalidManifest(
                "entry_points must define at least one of `wasm` or `ui`".into(),
            ));
        }

        let world = raw
            .plugin
            .world
            .unwrap_or_else(|| DEFAULT_WORLD.to_string());
        validate_world_identifier(&world)?;

        let targets = match raw.plugin.targets {
            Some(tokens) if tokens.is_empty() => {
                return Err(PluginError::InvalidManifest(
                    "plugin.targets must list at least one edition".into(),
                ));
            }
            Some(tokens) => tokens
                .into_iter()
                .map(|t| Edition::from_token(&t))
                .collect::<Result<Vec<_>, _>>()?,
            None => vec![Edition::Desktop, Edition::Web],
        };

        let restart_policy = match raw.plugin.restart_policy.as_deref() {
            Some(token) => RestartPolicy::from_token(token)?,
            None => RestartPolicy::default(),
        };

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
            world,
            targets,
            restart_policy,
            author: raw.plugin.author,
            license: raw.plugin.license,
            homepage: raw.plugin.homepage,
            description: raw.plugin.description,
        };

        let required = raw
            .permissions
            .required
            .into_iter()
            .map(RawPermissionGrant::into_grant)
            .collect::<Result<Vec<_>, _>>()?;
        let optional = raw
            .permissions
            .optional
            .into_iter()
            .map(RawPermissionGrant::into_grant)
            .collect::<Result<Vec<_>, _>>()?;

        let permissions = PermissionSet { required, optional };

        Ok(Self {
            plugin,
            permissions,
            entry_points: EntryPoints {
                wasm: raw.entry_points.wasm,
                ui: raw.entry_points.ui,
            },
            runtime: raw.runtime,
            contributions: {
                // Validate event-subscription patterns at parse time
                // (I6.2) — fail-loud beats failing silently at
                // activation when EventBus::subscribe rejects.
                for pattern in &raw.contributions.event_subscriptions {
                    crate::event_bus::tokenise_pattern(pattern).map_err(|err| {
                        PluginError::InvalidManifest(format!(
                            "contributions.event_subscriptions entry `{pattern}` invalid: {err}"
                        ))
                    })?;
                }
                raw.contributions
            },
        })
    }
}

/// Validates a fully qualified WIT world identifier of the shape
/// `<namespace>:<package>@<version>/<world>` — e.g.
/// `plamenix:plugin@1.0.0/plugin-minimal`. Catches typos at parse time
/// rather than leaving them to fail mysteriously at instantiation.
fn validate_world_identifier(world: &str) -> Result<(), PluginError> {
    let (qualified, world_name) = world.rsplit_once('/').ok_or_else(|| {
        PluginError::InvalidManifest(format!(
            "plugin.world `{world}` missing `/<world-name>` suffix"
        ))
    })?;
    if world_name.is_empty() {
        return Err(PluginError::InvalidManifest(format!(
            "plugin.world `{world}` has empty world name after `/`"
        )));
    }
    let (package, version) = qualified.rsplit_once('@').ok_or_else(|| {
        PluginError::InvalidManifest(format!(
            "plugin.world `{world}` missing `@<version>` between package and world name"
        ))
    })?;
    if package.is_empty() || !package.contains(':') {
        return Err(PluginError::InvalidManifest(format!(
            "plugin.world `{world}` package prefix must be `<namespace>:<package>`"
        )));
    }
    Version::parse(version).map_err(|err| {
        PluginError::InvalidManifest(format!(
            "plugin.world `{world}` version segment is not valid SemVer: {err}"
        ))
    })?;
    Ok(())
}
