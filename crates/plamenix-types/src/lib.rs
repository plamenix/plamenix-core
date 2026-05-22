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
use specta::Type;
use uuid::Uuid;

/// Identifier for an active Firebird session.
///
/// A session is created when [`ConnectionConfig`] is attached to a
/// Firebird server and dropped when the owning tab closes or the session
/// expires. Session identifiers are opaque UUIDs and must not be parsed
/// or generated outside the connection layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, Type)]
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
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, Type)]
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
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
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
    /// Optional encryption key for at-rest-encrypted databases.
    ///
    /// The Firebird C API supplies runtime keys through
    /// `fb_database_crypt_callback`, registered before
    /// `isc_attach_database`. Upstream rsfbclient `v0.26` does not
    /// surface that callback; Plamenix ships the binding in the
    /// vendored `rsfbclient-native` crate under
    /// `vendor/rsfbclient-native/src/crypt.rs`. `plamenix-db`'s
    /// connect path calls `set_key(...)` immediately before
    /// `isc_attach_database` and clears the slot once the attach
    /// settles, so the bytes do not stay resident between sessions.
    ///
    /// A KeyHolder plugin (e.g. IBSurgeon EPF) configured on the
    /// fbclient install can still supply keys via its own
    /// `fbcrypt.conf`; both paths coexist and the callback wins when
    /// both are present (Firebird checks the registered callback
    /// first).
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
    /// Wire charset for the session. `None` falls back to `UTF8`.
    ///
    /// Accepted values match `rsfbclient_core::Charset::from_str`:
    /// `UTF8`, `ISO8859_1`..`ISO8859_7`, `ISO8859_13`, `WIN1250`..`WIN1258`
    /// (no `1255`), `ASCII`, `KOI8R`, `KOI8U`, `EUCJP`, `BIG5_2003`.
    /// Picked at attach time and applied to every string column the
    /// driver decodes for the rest of the session — affects how legacy
    /// `WIN1250` databases render Croatian / Czech / Polish text.
    #[serde(default)]
    pub charset: Option<String>,
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
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Schema {
    /// All non-system tables and views the connection can see, sorted
    /// by name and each carrying their column list.
    pub tables: Vec<TableInfo>,
    /// Stored procedures from `RDB$PROCEDURES` (system flag = 0).
    #[serde(default)]
    pub procedures: Vec<ProcedureInfo>,
    /// Triggers from `RDB$TRIGGERS` (system flag = 0).
    #[serde(default)]
    pub triggers: Vec<TriggerInfo>,
    /// Sequences / generators from `RDB$GENERATORS`.
    #[serde(default)]
    pub generators: Vec<GeneratorInfo>,
    /// User-defined domains from `RDB$FIELDS` (excluding the auto
    /// `RDB$N`-named domains that back regular columns).
    #[serde(default)]
    pub domains: Vec<DomainInfo>,
}

/// One stored procedure.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ProcedureInfo {
    /// Procedure name as Firebird stores it.
    pub name: String,
    /// Number of declared input parameters.
    pub input_count: i32,
    /// Number of declared output parameters. `0` for executable
    /// procedures with no `RETURNS` clause.
    pub output_count: i32,
}

/// One trigger row.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TriggerInfo {
    /// Trigger name.
    pub name: String,
    /// Relation the trigger fires on (empty for database-level
    /// triggers).
    pub relation: String,
    /// Trigger type per `RDB$TRIGGERS.RDB$TRIGGER_TYPE`. Decoded by the
    /// UI into BEFORE/AFTER + INSERT/UPDATE/DELETE labels.
    pub trigger_type: i64,
    /// `true` when the trigger is currently active (the inverse of
    /// `RDB$TRIGGER_INACTIVE`).
    pub active: bool,
}

/// One sequence / generator.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct GeneratorInfo {
    /// Generator name.
    pub name: String,
    /// Current value, sampled via `GEN_ID(<name>, 0)` at describe-schema
    /// time. Surfaced by the welcome dashboard and the inline editor in
    /// the schema browser. `0` for a freshly created generator that has
    /// never been incremented.
    pub current_value: i64,
}

/// One user-defined domain.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DomainInfo {
    /// Domain name.
    pub name: String,
    /// Rendered SQL type (e.g. `VARCHAR(50)`, `INTEGER`).
    pub sql_type: String,
    /// `true` when the domain allows NULL.
    pub nullable: bool,
}

/// One row from `RDB$RELATIONS` plus its columns.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TableInfo {
    /// Relation name as Firebird stores it (already trimmed of CHAR
    /// padding).
    pub name: String,
    /// Whether this is a persistent table or a view.
    pub kind: TableKind,
    /// Column metadata, in declared position order.
    pub columns: Vec<ColumnInfo>,
    /// Primary-key column names, in declared segment order. Empty for
    /// tables without a `PRIMARY KEY` constraint or for views.
    #[serde(default)]
    pub primary_key: Vec<String>,
}

/// Persistent table vs view, derived from `RDB$RELATION_TYPE`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum TableKind {
    /// Persistent base table.
    Table,
    /// View.
    View,
}

/// One row from `RDB$RELATION_FIELDS`/`RDB$FIELDS`.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
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
    /// Default-value expression as declared in the DDL, with the
    /// leading `DEFAULT ` keyword stripped. `None` when no default is
    /// set. Sourced from `RDB$RELATION_FIELDS.RDB$DEFAULT_SOURCE`
    /// first (per-column default) and falls back to
    /// `RDB$FIELDS.RDB$DEFAULT_SOURCE` (domain default).
    #[serde(default)]
    pub default_expr: Option<String>,
}

/// Result of a `db_test_connection` request.
///
/// Always returns shape (no thrown error on a clean failure); the `ok`
/// flag distinguishes success from a reported failure. `firebird_version`
/// is best-effort and comes from
/// `rdb$get_context('SYSTEM', 'ENGINE_VERSION')` when the attach
/// succeeded.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TestConnectionResult {
    /// `true` when the attach succeeded.
    pub ok: bool,
    /// Firebird engine version reported by the server, when reachable.
    pub firebird_version: Option<String>,
    /// Failure message when `ok` is `false`.
    pub error: Option<String>,
    /// Wall-clock duration of the attach (and version probe) in
    /// milliseconds.
    pub duration_ms: u64,
}

/// One entry from the per-profile SQL query history.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// Auto-incrementing primary key from the local SQLite store.
    pub id: i64,
    /// Profile this entry belongs to.
    pub profile_id: String,
    /// Statement text as executed.
    pub sql: String,
    /// Epoch milliseconds (UTC) when the statement ran.
    pub executed_at: i64,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// `ok` or `err`.
    pub status: String,
    /// Error string when `status` is `err`.
    pub error: Option<String>,
    /// Row count for SELECT-like outcomes; `None` for DML/DDL.
    pub row_count: Option<i64>,
    /// Free-form user-assigned label. `None` means the entry is shown
    /// by its SQL preview only; a non-empty string renders as a chip
    /// above the preview in the history panel.
    pub label: Option<String>,
}

/// One alias entry from a Firebird `databases.conf` file.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseAlias {
    /// The alias name (left-hand side of `name = path`).
    pub alias: String,
    /// The database path (right-hand side).
    pub path: String,
}

/// Result of `db_list_aliases`. `source_path` is the path of the parsed
/// `databases.conf`, or `None` when no candidate file was found on the
/// local filesystem (e.g. when the IDE is running on a different host
/// than the Firebird server).
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ListAliasesResult {
    /// Absolute path of the `databases.conf` that was parsed.
    pub source_path: Option<String>,
    /// Aliases parsed from the file.
    pub aliases: Vec<DatabaseAlias>,
}

/// Snapshot of `MON$DATABASE`, plus the live attachment and statement
/// lists for the dashboard.
///
/// All counters are stamped at query time and not refreshed; the UI
/// pulls a new [`DatabaseStats`] when the user clicks "Refresh".
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseStats {
    /// `MON$DATABASE` row plus a couple of derived counters.
    pub database: MonDatabase,
    /// All live attachments visible to the connected user.
    pub attachments: Vec<AttachmentInfo>,
    /// All live statements visible to the connected user.
    pub statements: Vec<StatementInfo>,
}

/// One row of `MON$DATABASE` plus the corresponding `MON$IO_STATS` /
/// `MON$RECORD_STATS` totals at the database level.
///
/// Field names mirror the Firebird metadata column names with the
/// `MON$` prefix dropped and converted to `snake_case` for serde + Specta.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct MonDatabase {
    /// `MON$DATABASE_NAME` — full database path on the server.
    pub name: String,
    /// `MON$PAGE_SIZE`.
    pub page_size: i64,
    /// `MON$PAGES` — pages currently allocated.
    pub pages: i64,
    /// `MON$OLDEST_TRANSACTION` (OIT).
    pub oldest_transaction: i64,
    /// `MON$OLDEST_ACTIVE` (OAT).
    pub oldest_active: i64,
    /// `MON$OLDEST_SNAPSHOT` (OST).
    pub oldest_snapshot: i64,
    /// `MON$NEXT_TRANSACTION` (Next).
    pub next_transaction: i64,
    /// `MON$SWEEP_INTERVAL`.
    pub sweep_interval: i64,
    /// `true` when `MON$FORCED_WRITES = 1`.
    pub forced_writes: bool,
    /// `true` when `MON$READ_ONLY = 1`.
    pub read_only: bool,
    /// `MON$SQL_DIALECT` — usually `3`.
    pub sql_dialect: i64,
    /// `MON$CREATION_DATE` rendered as ISO string.
    pub creation_date: String,
    /// `MON$BACKUP_STATE` — 0 normal, 1 stalled, 2 merge.
    pub backup_state: i64,
    /// `MON$SHUTDOWN_MODE` — 0 online, 1 multi, 2 single, 3 full.
    pub shutdown_mode: i64,
    /// `MON$OWNER` — database owner.
    pub owner: String,
    /// Firebird engine version reported by `rdb$get_context`.
    pub server_version: String,
    /// `MON$ODS_MAJOR`.
    pub ods_major: i64,
    /// `MON$ODS_MINOR`.
    pub ods_minor: i64,
    /// Total reads charged to the database (`MON$IO_STATS` aggregate).
    pub reads: i64,
    /// Total writes charged to the database.
    pub writes: i64,
    /// Total fetches.
    pub fetches: i64,
    /// Total marks.
    pub marks: i64,
}

/// One live attachment seen via `MON$ATTACHMENTS`.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentInfo {
    /// `MON$ATTACHMENT_ID`.
    pub id: i64,
    /// `MON$USER`.
    pub user: String,
    /// `MON$ROLE`, blank string when no explicit role.
    pub role: String,
    /// `MON$REMOTE_ADDRESS`.
    pub remote_address: String,
    /// `MON$REMOTE_PROCESS` — friendly name of the client binary.
    pub remote_process: String,
    /// `MON$CHARACTER_SET_ID` looked up to its character-set name.
    pub character_set: String,
    /// `MON$STATE` — 0 idle, 1 active, 2 stalled.
    pub state: i64,
    /// `MON$TIMESTAMP` rendered as ISO string.
    pub timestamp: String,
    /// `true` when this attachment is the one querying `MON$ATTACHMENTS`.
    pub is_self: bool,
}

/// One live statement seen via `MON$STATEMENTS`.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct StatementInfo {
    /// `MON$STATEMENT_ID`.
    pub id: i64,
    /// `MON$ATTACHMENT_ID` that owns the statement.
    pub attachment_id: i64,
    /// `MON$STATE` — 0 idle, 1 active, 2 stalled.
    pub state: i64,
    /// `MON$TIMESTAMP` rendered as ISO string.
    pub timestamp: String,
    /// `MON$SQL_TEXT` — the prepared SQL for the statement.
    pub sql_text: String,
}
