//! Capability grammar.
//!
//! Parses the dotted capability strings declared in `manifest.toml`
//! into a typed [`Permission`] enum. See `../../../docs/capability-model.md`
//! for the full grammar; this implementation covers the M1 subset and
//! keeps an `Other` variant so unknown capabilities surface as
//! `InvalidCapability` from [`Permission::parse`] rather than silently
//! granting access.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::PluginError;

/// A single capability granted to a plugin.
///
/// Variants are arranged by resource (db / net / fs / auth / clipboard /
/// os / runtime). Scoped variants (e.g. `db.read.table.<name>`) carry
/// their scope inline so a permission check can compare the whole value
/// without re-parsing.
///
/// Variant names map 1:1 to capability strings documented in
/// `docs/capability-model.md`; the grammar doc on this enum covers
/// their meaning, so per-variant doc comments are omitted.
#[allow(missing_docs)]
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum Permission {
    DbReadAny,
    DbReadTable(String),
    DbWriteAny,
    DbWriteTable(String),
    DbDdlAny,
    DbDdlTable(String),
    DbSchemaList,
    DbSchemaDescribe,
    ExportFormat,
    ImportSource,
    NetHttps,
    NetHttpsHost(String),
    NetHttp,
    FsReadDir(LogicalDir),
    FsWriteDir(LogicalDir),
    AuthOsKeyring(OsKeyring),
    ClipboardRead,
    ClipboardWrite,
    OsNotify,
    OsOpenUrl,
}

/// Whitelisted logical directories that plugins may read or write.
///
/// Absolute paths are never granted directly. The host resolves the
/// alias to an OS path at runtime; on web, where filesystem access is
/// unavailable, calls into these resources fail with
/// `PermissionDenied`.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogicalDir {
    Downloads,
    Documents,
    Temp,
    PluginData,
    PluginConfig,
}

impl LogicalDir {
    fn from_token(token: &str) -> Result<Self, PluginError> {
        match token {
            "downloads" => Ok(Self::Downloads),
            "documents" => Ok(Self::Documents),
            "temp" => Ok(Self::Temp),
            "plugin-data" => Ok(Self::PluginData),
            "plugin-config" => Ok(Self::PluginConfig),
            other => Err(PluginError::InvalidCapability(
                other.to_owned(),
                "unknown logical directory",
            )),
        }
    }

    const fn as_token(self) -> &'static str {
        match self {
            Self::Downloads => "downloads",
            Self::Documents => "documents",
            Self::Temp => "temp",
            Self::PluginData => "plugin-data",
            Self::PluginConfig => "plugin-config",
        }
    }
}

/// Platform-specific keyring backends.
#[allow(missing_docs)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OsKeyring {
    Windows,
    Macos,
    LinuxKeyring,
}

impl OsKeyring {
    fn from_token(token: &str) -> Result<Self, PluginError> {
        match token {
            "windows" => Ok(Self::Windows),
            "macos" => Ok(Self::Macos),
            "linux-keyring" => Ok(Self::LinuxKeyring),
            other => Err(PluginError::InvalidCapability(
                other.to_owned(),
                "unknown keyring backend",
            )),
        }
    }

    const fn as_token(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::LinuxKeyring => "linux-keyring",
        }
    }
}

impl Permission {
    /// Parses a dotted capability string into a [`Permission`].
    ///
    /// Wildcards are deliberately not supported; the manifest must list
    /// every capability it relies on. See
    /// `docs/capability-model.md`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::InvalidCapability`] when the input does
    /// not match any rule in the grammar (including unknown logical
    /// directories or keyring backends).
    pub fn parse(raw: &str) -> Result<Self, PluginError> {
        let parts: Vec<&str> = raw.split('.').collect();
        match parts.as_slice() {
            ["db", "read", "any"] => Ok(Self::DbReadAny),
            ["db", "read", "table", name] => Ok(Self::DbReadTable((*name).to_owned())),
            ["db", "write", "any"] => Ok(Self::DbWriteAny),
            ["db", "write", "table", name] => Ok(Self::DbWriteTable((*name).to_owned())),
            ["db", "ddl", "any"] => Ok(Self::DbDdlAny),
            ["db", "ddl", "table", name] => Ok(Self::DbDdlTable((*name).to_owned())),
            ["db", "schema", "list"] => Ok(Self::DbSchemaList),
            ["db", "schema", "describe"] => Ok(Self::DbSchemaDescribe),
            ["export", "format"] => Ok(Self::ExportFormat),
            ["import", "source"] => Ok(Self::ImportSource),
            ["net", "https"] => Ok(Self::NetHttps),
            ["net", "https", host @ ..] if !host.is_empty() => {
                Ok(Self::NetHttpsHost(host.join(".")))
            }
            ["net", "http"] => Ok(Self::NetHttp),
            ["fs", "read", "dir", dir] => Ok(Self::FsReadDir(LogicalDir::from_token(dir)?)),
            ["fs", "write", "dir", dir] => Ok(Self::FsWriteDir(LogicalDir::from_token(dir)?)),
            ["auth", "os", backend] => Ok(Self::AuthOsKeyring(OsKeyring::from_token(backend)?)),
            ["clipboard", "read"] => Ok(Self::ClipboardRead),
            ["clipboard", "write"] => Ok(Self::ClipboardWrite),
            ["os", "notify"] => Ok(Self::OsNotify),
            ["os", "open-url"] => Ok(Self::OsOpenUrl),
            _ => Err(PluginError::InvalidCapability(
                raw.to_owned(),
                "no matching capability rule",
            )),
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DbReadAny => f.write_str("db.read.any"),
            Self::DbReadTable(name) => write!(f, "db.read.table.{name}"),
            Self::DbWriteAny => f.write_str("db.write.any"),
            Self::DbWriteTable(name) => write!(f, "db.write.table.{name}"),
            Self::DbDdlAny => f.write_str("db.ddl.any"),
            Self::DbDdlTable(name) => write!(f, "db.ddl.table.{name}"),
            Self::DbSchemaList => f.write_str("db.schema.list"),
            Self::DbSchemaDescribe => f.write_str("db.schema.describe"),
            Self::ExportFormat => f.write_str("export.format"),
            Self::ImportSource => f.write_str("import.source"),
            Self::NetHttps => f.write_str("net.https"),
            Self::NetHttpsHost(host) => write!(f, "net.https.{host}"),
            Self::NetHttp => f.write_str("net.http"),
            Self::FsReadDir(dir) => write!(f, "fs.read.dir.{}", (*dir).as_token()),
            Self::FsWriteDir(dir) => write!(f, "fs.write.dir.{}", (*dir).as_token()),
            Self::AuthOsKeyring(backend) => write!(f, "auth.os.{}", (*backend).as_token()),
            Self::ClipboardRead => f.write_str("clipboard.read"),
            Self::ClipboardWrite => f.write_str("clipboard.write"),
            Self::OsNotify => f.write_str("os.notify"),
            Self::OsOpenUrl => f.write_str("os.open-url"),
        }
    }
}

/// One declared capability + optional human-readable rationale.
///
/// Mirrors the iOS `Info.plist` *Usage Description* pattern: every
/// capability the plugin requests carries a sentence explaining
/// *why*, shown in the install dialog and the Permissions panel.
/// Optional: a plugin may ship purpose-less grants, in which case the
/// install dialog shows the capability without a rationale and the
/// user judges it on the capability alone.
///
/// The grammar shape supports two TOML forms:
///
/// ```toml
/// # Plain string — no purpose given
/// required = ["db.schema.list"]
///
/// # Object form — purpose attached
/// required = [
///   { capability = "db.schema.list", purpose = "List tables to choose from" }
/// ]
///
/// # Mixed list — both forms accepted in the same array
/// required = [
///   "db.schema.list",
///   { capability = "fs.write.dir.plugin-data", purpose = "Cache export progress" }
/// ]
/// ```
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct PermissionGrant {
    /// The capability the plugin is asking for.
    pub capability: Permission,
    /// One-line rationale shown in the install dialog and Permissions
    /// panel. `None` when the author did not supply one; `plamenix-cli
    /// validate` warns but does not reject, since the capability itself
    /// is what the host enforces.
    pub purpose: Option<String>,
}

impl PermissionGrant {
    /// Constructs a grant without a purpose string (legacy / dev /
    /// sideload-shaped grant — equivalent to the TOML plain-string
    /// form).
    #[must_use]
    pub fn new(capability: Permission) -> Self {
        Self {
            capability,
            purpose: None,
        }
    }

    /// Constructs a grant with an attached rationale.
    #[must_use]
    pub fn with_purpose(capability: Permission, purpose: impl Into<String>) -> Self {
        Self {
            capability,
            purpose: Some(purpose.into()),
        }
    }
}

/// The set of permissions a plugin has been granted at install time.
///
/// `required` permissions are non-negotiable; the host refuses to load
/// a plugin whose required set has been denied. `optional` permissions
/// surface as a user-facing prompt before grant. Each entry is a
/// [`PermissionGrant`] carrying the capability plus an optional
/// purpose string (Info.plist analog) so the install dialog can show
/// rationale text per capability.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PermissionSet {
    /// Permissions the plugin cannot operate without.
    pub required: Vec<PermissionGrant>,
    /// Permissions the plugin would like but can run without.
    pub optional: Vec<PermissionGrant>,
}

impl PermissionSet {
    /// Returns `true` when either bucket contains the given capability.
    #[must_use]
    pub fn grants(&self, permission: &Permission) -> bool {
        self.required.iter().any(|g| &g.capability == permission)
            || self.optional.iter().any(|g| &g.capability == permission)
    }

    /// Iterator over capabilities in the required bucket (ignores
    /// purpose strings). Convenience for callers that only need the
    /// capability list — e.g. an activator passes the
    /// joined capability strings to the plugin binary's `--permissions`
    /// argument.
    pub fn required_caps(&self) -> impl Iterator<Item = &Permission> {
        self.required.iter().map(|g| &g.capability)
    }

    /// Iterator over capabilities in the optional bucket (ignores
    /// purpose strings).
    pub fn optional_caps(&self) -> impl Iterator<Item = &Permission> {
        self.optional.iter().map(|g| &g.capability)
    }
}
