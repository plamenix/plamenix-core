//! Shared types for the Plamenix Firebird IDE.
//!
//! This crate owns the small set of value types that cross every module
//! boundary in Plamenix: connection configuration, session and tab
//! identifiers, and the few shapes that the IPC layer serialises in both
//! directions. The crate has no IO and no dependencies beyond [`serde`]
//! and [`uuid`]; it is safe to depend on from every other Plamenix crate.
//!
//! Types accrete here when a second crate needs them. Domain logic does
//! not live in this crate — only the data it operates on.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Identifier for an active Firebird session.
///
/// A session is created when [`ConnectionConfig`] is attached to a
/// Firebird server and dropped when the owning tab closes or the session
/// expires. Session identifiers are opaque UUIDs and must not be parsed
/// or generated outside the connection layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Generates a fresh, random session identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Identifier for an open UI tab.
///
/// Each tab owns its own [`SessionId`] and per-tab state (selection,
/// filters, pagination, scroll position). Tab identifiers are stable
/// across tab reorders.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct TabId(pub Uuid);

impl TabId {
    /// Generates a fresh, random tab identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TabId {
    fn default() -> Self {
        Self::new()
    }
}

/// Connection configuration for attaching to a Firebird database.
///
/// Captures every parameter the connect dialog collects, including
/// encryption-related fields used by Firebird 3+ `DbCrypt` / `KeyHolder`
/// plugins. Passwords and encryption keys travel through this type but
/// are never persisted: the connection layer hands them to the driver
/// and clears its copy as soon as the attach succeeds.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectionConfig {
    /// Hostname or IP address of the Firebird server.
    pub host: String,
    /// TCP port. Default Firebird port is `3050`.
    pub port: u16,
    /// Database path or registered alias on the server.
    pub database: String,
    /// Username to authenticate with.
    pub user: String,
    /// Password to authenticate with. Held in memory only.
    pub password: String,
    /// Optional encryption key for encrypted databases.
    ///
    /// When the target database is encrypted, the user supplies a key
    /// here. The native `fbclient` either passes the key directly via
    /// the DPB or invokes a key-holder callback that returns it.
    #[serde(default)]
    pub encryption_key: Option<String>,
    /// Optional override for the `fbclient` library path.
    ///
    /// When `None`, Plamenix uses the bundled `fbclient` for the user's
    /// platform. When `Some`, the path points at a user-supplied library
    /// (e.g. a different Firebird major version) loaded via
    /// `rsfbclient`'s `with_dyn_load` builder.
    #[serde(default)]
    pub fbclient_path: Option<String>,
    /// If `true`, refuse to connect to a database whose `MON$CRYPT_STATE`
    /// is not `1` (encrypted). Defends against accidental connections to
    /// unencrypted environments when the user expects an encrypted one.
    #[serde(default)]
    pub encryption_required: bool,
}

/// Catalogue describing the schema visible to a Firebird session.
///
/// Returned by `DbDriver::describe_schema`. Includes tables and views
/// (filtered by `RDB$SYSTEM_FLAG = 0`); system relations are excluded.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schema {
    /// All non-system tables and views the connection can see, sorted
    /// by name and each carrying their column list.
    pub tables: Vec<TableInfo>,
}

/// One row from `RDB$RELATIONS` plus its columns.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableInfo {
    /// Relation name as Firebird stores it (already trimmed of CHAR
    /// padding).
    pub name: String,
    /// Whether this is a persistent table or a view.
    pub kind: TableKind,
    /// Column metadata, in declared position order.
    pub columns: Vec<ColumnInfo>,
}

/// Persistent table vs view, derived from `RDB$RELATION_TYPE`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableKind {
    /// Persistent base table.
    Table,
    /// View.
    View,
}

/// One row from `RDB$RELATION_FIELDS`/`RDB$FIELDS`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnInfo {
    /// Column name as Firebird stores it.
    pub name: String,
    /// Ordinal position within the relation, starting at 0.
    pub position: i32,
    /// Human-readable SQL type string (e.g. `VARCHAR(50)`,
    /// `TIMESTAMP WITH TIME ZONE`, `BLOB SUB_TYPE TEXT`).
    pub sql_type: String,
    /// `true` when the column allows `NULL`.
    pub nullable: bool,
}
