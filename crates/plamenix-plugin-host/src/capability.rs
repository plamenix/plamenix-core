//! Capability grammar.
//!
//! Parses the dotted capability strings declared in `manifest.toml`
//! into a typed [`Permission`] enum. See `../../../docs/capability-model.md`
//! for the full grammar; this implementation covers the M1 subset and
//! keeps an `Other` variant so unknown capabilities surface as
//! `InvalidCapability` from [`Permission::parse`] rather than silently
//! granting access.

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
    RuntimeSubprocess,
}

/// Whitelisted logical directories that plugins may read or write.
///
/// Absolute paths are never granted directly. The host resolves the
/// alias to an OS path at runtime; on web, where filesystem access is
/// unavailable, calls into these resources fail with
/// `PermissionDenied`.
#[allow(missing_docs)]
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
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
}

/// Platform-specific keyring backends.
#[allow(missing_docs)]
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
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
            ["runtime", "subprocess"] => Ok(Self::RuntimeSubprocess),
            _ => Err(PluginError::InvalidCapability(
                raw.to_owned(),
                "no matching capability rule",
            )),
        }
    }
}

/// The set of permissions a plugin has been granted at install time.
///
/// `required` permissions are non-negotiable; the host refuses to load
/// a plugin whose required set has been denied. `optional` permissions
/// surface as a user-facing prompt before grant.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PermissionSet {
    /// Permissions the plugin cannot operate without.
    pub required: Vec<Permission>,
    /// Permissions the plugin would like but can run without.
    pub optional: Vec<Permission>,
}

impl PermissionSet {
    /// Returns `true` when the set contains the given permission in
    /// either the required or optional bucket.
    #[must_use]
    pub fn grants(&self, permission: &Permission) -> bool {
        self.required.contains(permission) || self.optional.contains(permission)
    }
}
