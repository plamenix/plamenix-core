//! `rsfbclient`-backed implementation of [`crate::DbDriver`].
//!
//! Two backends are exposed through one driver:
//!
//! * `native` (default) — `builder_native().with_dyn_load(path)`, the
//!   canonical Plamenix mode. The fbclient library shipped per platform
//!   in the installer is loaded at runtime.
//! * `pure-rust` — `builder_pure_rust()`, kept as a fallback when no
//!   fbclient is available.
//!
//! The driver owns a `HashMap<SessionId, Arc<Mutex<SimpleConnection>>>`.
//! Each connection lives behind its own mutex, which doubles as the
//! per-session "one in-flight call" lock from the MVP (rsfbclient +
//! fbclient are not safe to call concurrently against one attachment).
//! Synchronous rsfbclient calls run inside `tokio::task::spawn_blocking`;
//! the blocking task acquires the mutex via `blocking_lock` so the
//! same lock used by async callers serialises against the worker.

pub mod resolver;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use plamenix_types::{
    AttachmentInfo, ColumnInfo, ConnectionConfig, DatabaseStats, DomainInfo, GeneratorInfo,
    MonDatabase, ProcedureInfo, Schema, SessionId, StatementInfo, TableInfo, TableKind,
    TriggerInfo,
};
use rsfbclient::{Execute, Queryable, Row as RsfbRow, SimpleConnection, SqlType};
use tokio::sync::Mutex;

use crate::crypt::CryptState;
use crate::driver::{ConnectMode, DbDriver};
use crate::error::DbError;
use crate::query::{BlobRef, Column, ColumnValue, QueryResult, Row};

type SharedConn = Arc<Mutex<SimpleConnection>>;

/// Length in bytes of the BLOB peek preview surfaced inline with each
/// result cell. Enough for MIME-sniffing and a hex chip in the table
/// without inflating the row payload for large BLOBs.
const BLOB_PEEK_BYTES: usize = 32;

/// Per-session cache of BLOB bodies. Keyed by the opaque id returned
/// in [`BlobRef`]. Each execute clears the previous bin so memory
/// does not accumulate across queries; closing the session drops the
/// entry entirely.
type BlobBin = HashMap<String, Vec<u8>>;

/// The default Plamenix driver. Cheap to clone; cloning shares the
/// session registry through an `Arc`.
#[derive(Clone, Default)]
pub struct RsfbDriver {
    sessions: Arc<Mutex<HashMap<SessionId, SharedConn>>>,
    blobs: Arc<Mutex<HashMap<SessionId, BlobBin>>>,
}

impl RsfbDriver {
    /// Returns a new, empty driver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    async fn shared_conn(&self, session: SessionId) -> Result<SharedConn, DbError> {
        let sessions = self.sessions.lock().await;
        sessions
            .get(&session)
            .cloned()
            .ok_or_else(|| DbError::Driver(format!("unknown session: {session:?}")))
    }
}

#[async_trait]
impl DbDriver for RsfbDriver {
    #[tracing::instrument(
        name = "db.connect",
        skip(self, config),
        fields(host = %config.host, database = %config.database, mode = ?mode),
    )]
    async fn connect(
        &self,
        config: ConnectionConfig,
        mode: ConnectMode,
    ) -> Result<SessionId, DbError> {
        let encryption_required = config.encryption_required;
        // Wire the user-supplied key into fbclient's runtime callback
        // before the attach handshake fires. Native mode only — the
        // pure-Rust backend never loads fbclient. rsfbclient 0.26
        // doesn't expose `fb_database_crypt_callback`, so the bridge
        // in `crate::crypt_callback` dlopens the same library
        // rsfbclient uses and registers a Rust callback that reads
        // from a process-global key slot.
        #[cfg(feature = "native")]
        if matches!(mode, ConnectMode::Native)
            && let Some(key) = config.encryption_key.as_deref()
            && !key.is_empty()
        {
            let resolved = crate::rsfb::resolver::resolve_fbclient_path(&config);
            match resolved {
                Some(path) => {
                    if let Err(err) = crate::crypt_callback::register_with(&path) {
                        tracing::warn!(
                            ?err,
                            "could not register fbclient crypt callback; \
                             database attach will rely on KeyHolder plugins instead",
                        );
                    } else {
                        crate::crypt_callback::set_key(key.as_bytes());
                    }
                }
                None => {
                    tracing::warn!(
                        "encryption_key supplied but no fbclient path resolved; \
                         set PLAMENIX_FBCLIENT_PATH or `fbclient_path` to enable \
                         runtime key forwarding",
                    );
                }
            }
        }

        let conn = tokio::task::spawn_blocking(move || build_connection(&config, &mode)).await??;

        // Best-effort wipe of the process-global key slot once the
        // attach handshake completes (success or failure). Leaves
        // memory pressure lower than holding the key for the
        // remainder of the process lifetime.
        #[cfg(feature = "native")]
        crate::crypt_callback::clear_key();

        let id = SessionId::new();
        let shared = Arc::new(Mutex::new(conn));

        self.sessions.lock().await.insert(id, shared);
        tracing::info!(?id, "session attached");

        if encryption_required {
            match self.crypt_state(id).await {
                Ok(state) if state.is_encrypted() => {}
                Ok(state) => {
                    let _ = self.close(id).await;
                    return Err(DbError::Connect(format!(
                        "encryption_required: database MON$CRYPT_STATE is {state:?}",
                    )));
                }
                Err(err) => {
                    let _ = self.close(id).await;
                    return Err(DbError::Connect(format!(
                        "encryption_required: could not read MON$CRYPT_STATE: {err}",
                    )));
                }
            }
        }

        Ok(id)
    }

    #[tracing::instrument(
        name = "db.execute",
        skip(self, sql),
        fields(session = %session.0, sql_len = sql.len()),
    )]
    async fn execute(&self, session: SessionId, sql: String) -> Result<QueryResult, DbError> {
        let shared = self.shared_conn(session).await?;
        let blobs_arc = Arc::clone(&self.blobs);
        let (result, bin) =
            tokio::task::spawn_blocking(move || -> Result<(QueryResult, BlobBin), DbError> {
                let mut guard = shared.blocking_lock();
                let mut bin: BlobBin = HashMap::new();
                let result = run_statement(&mut guard, &sql, &mut bin)?;
                Ok((result, bin))
            })
            .await??;
        // Replace any previous BLOB cache for this session — fresh
        // execute, fresh handles.
        blobs_arc.lock().await.insert(session, bin);
        Ok(result)
    }

    #[tracing::instrument(name = "db.ping", skip(self), fields(session = %session.0))]
    async fn ping(&self, session: SessionId) -> Result<String, DbError> {
        let shared = self.shared_conn(session).await?;
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.blocking_lock();
            run_ping(&mut guard)
        })
        .await?
    }

    #[tracing::instrument(name = "db.close", skip(self), fields(session = %session.0))]
    async fn close(&self, session: SessionId) -> Result<(), DbError> {
        let mut sessions = self.sessions.lock().await;
        if sessions.remove(&session).is_some() {
            self.blobs.lock().await.remove(&session);
            tracing::info!(?session, "session detached");
            Ok(())
        } else {
            Err(DbError::Driver(format!("unknown session: {session:?}")))
        }
    }

    #[tracing::instrument(name = "db.fetch_blob", skip(self), fields(session = %session.0, blob = %blob_id))]
    async fn fetch_blob(&self, session: SessionId, blob_id: String) -> Result<Vec<u8>, DbError> {
        let blobs = self.blobs.lock().await;
        blobs
            .get(&session)
            .and_then(|bin| bin.get(&blob_id))
            .cloned()
            .ok_or_else(|| {
                DbError::Driver(format!(
                    "blob not cached (session {session:?} / id {blob_id}); re-run the query"
                ))
            })
    }

    #[tracing::instrument(name = "db.crypt_state", skip(self), fields(session = %session.0))]
    async fn crypt_state(&self, session: SessionId) -> Result<CryptState, DbError> {
        let shared = self.shared_conn(session).await?;
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.blocking_lock();
            run_crypt_state(&mut guard)
        })
        .await?
    }

    #[tracing::instrument(
        name = "db.describe_schema",
        skip(self),
        fields(session = %session.0),
    )]
    async fn describe_schema(&self, session: SessionId) -> Result<Schema, DbError> {
        let shared = self.shared_conn(session).await?;
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.blocking_lock();
            run_describe_schema(&mut guard)
        })
        .await?
    }

    #[tracing::instrument(
        name = "db.database_stats",
        skip(self),
        fields(session = %session.0),
    )]
    async fn database_stats(&self, session: SessionId) -> Result<DatabaseStats, DbError> {
        let shared = self.shared_conn(session).await?;
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.blocking_lock();
            run_database_stats(&mut guard)
        })
        .await?
    }
}

fn build_connection(
    config: &ConnectionConfig,
    mode: &ConnectMode,
) -> Result<SimpleConnection, DbError> {
    match mode {
        ConnectMode::Native => build_native(config),
        ConnectMode::PureRust => build_pure_rust(config),
    }
}

#[cfg(feature = "native")]
fn build_native(config: &ConnectionConfig) -> Result<SimpleConnection, DbError> {
    let Some(path) = resolver::resolve_fbclient_path(config) else {
        return Err(DbError::Connect(
            "native mode requires a bundled fbclient: set ConnectionConfig.fbclient_path \
             or the PLAMENIX_FBCLIENT_PATH environment variable"
                .into(),
        ));
    };
    let path_str = path.to_string_lossy().into_owned();
    if config.embedded {
        // Embedded Firebird authenticates locally — no password
        // needed. The user field is honored so MON$ATTACHMENTS still
        // records who connected.
        let mut builder = rsfbclient::builder_native()
            .with_dyn_load(path_str)
            .with_embedded();
        builder.db_name(&config.database);
        builder.user(&config.user);
        apply_charset_embedded(&mut builder, config)?;
        return builder
            .connect()
            .map(SimpleConnection::from)
            .map_err(|err| DbError::Connect(err.to_string()));
    }
    let mut builder = rsfbclient::builder_native()
        .with_dyn_load(path_str)
        .with_remote();
    builder
        .host(&config.host)
        .port(config.port)
        .db_name(&config.database)
        .user(&config.user)
        .pass(&config.password);
    apply_charset(&mut builder, config)?;
    builder
        .connect()
        .map(SimpleConnection::from)
        .map_err(|err| DbError::Connect(err.to_string()))
}

#[cfg(feature = "native")]
fn apply_charset_embedded<A, B>(
    builder: &mut rsfbclient::NativeConnectionBuilder<A, B>,
    config: &ConnectionConfig,
) -> Result<(), DbError>
where
    A: rsfbclient::ConfiguredLinkage,
    B: rsfbclient::ConfiguredConnType,
{
    apply_charset(builder, config)
}

#[cfg(feature = "native")]
fn apply_charset<A, B>(
    builder: &mut rsfbclient::NativeConnectionBuilder<A, B>,
    config: &ConnectionConfig,
) -> Result<(), DbError>
where
    A: rsfbclient::ConfiguredLinkage,
    B: rsfbclient::ConfiguredConnType,
{
    use std::str::FromStr;
    let Some(name) = config.charset.as_deref() else {
        return Ok(());
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let charset = rsfbclient::Charset::from_str(trimmed)
        .map_err(|err| DbError::Connect(format!("invalid charset '{trimmed}': {err}")))?;
    builder.charset(charset);
    Ok(())
}

#[cfg(not(feature = "native"))]
fn build_native(_config: &ConnectionConfig) -> Result<SimpleConnection, DbError> {
    Err(DbError::Driver(
        "native backend not compiled in (enable the `native` feature)".into(),
    ))
}

#[cfg(feature = "pure-rust")]
fn build_pure_rust(config: &ConnectionConfig) -> Result<SimpleConnection, DbError> {
    use std::str::FromStr;
    if config.embedded {
        return Err(DbError::Connect(
            "embedded mode is unsupported on the pure-rust driver — switch to native mode in the connection form"
                .into(),
        ));
    }
    let mut builder = rsfbclient::builder_pure_rust();
    builder
        .host(&config.host)
        .port(config.port)
        .db_name(&config.database)
        .user(&config.user)
        .pass(&config.password);
    if let Some(name) = config.charset.as_deref() {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            let charset = rsfbclient::Charset::from_str(trimmed)
                .map_err(|err| DbError::Connect(format!("invalid charset '{trimmed}': {err}")))?;
            builder.charset(charset);
        }
    }
    builder
        .connect()
        .map(SimpleConnection::from)
        .map_err(|err| DbError::Connect(err.to_string()))
}

#[cfg(not(feature = "pure-rust"))]
fn build_pure_rust(_config: &ConnectionConfig) -> Result<SimpleConnection, DbError> {
    Err(DbError::Driver(
        "pure-rust backend not compiled in (enable the `pure-rust` feature)".into(),
    ))
}

fn run_statement(
    conn: &mut SimpleConnection,
    sql: &str,
    bin: &mut BlobBin,
) -> Result<QueryResult, DbError> {
    // Heuristic for SELECT vs DML. Real prepared-statement metadata would
    // be more robust; this matches the MVP's behaviour and is good enough
    // until prepared-statement plumbing lands.
    let trimmed = sql.trim_start();
    let is_select = trimmed.split_whitespace().next().is_some_and(|word| {
        word.eq_ignore_ascii_case("SELECT") || word.eq_ignore_ascii_case("WITH")
    });

    if !is_select {
        let affected = conn.execute(sql, ()).map_err(DbError::from)?;
        return Ok(QueryResult::Affected {
            rows: u64::try_from(affected).unwrap_or(0),
        });
    }

    let rows: Vec<RsfbRow> = conn.query(sql, ()).map_err(DbError::from)?;
    let columns = rows
        .first()
        .map(|row| {
            row.cols
                .iter()
                .map(|c| Column {
                    name: c.name.clone(),
                })
                .collect()
        })
        .unwrap_or_default();

    let mapped_rows = rows
        .into_iter()
        .map(|row| Row {
            cells: row.cols.iter().map(|c| column_to_value(c, bin)).collect(),
        })
        .collect();

    Ok(QueryResult::Rows {
        columns,
        rows: mapped_rows,
        truncated: false,
    })
}

fn run_crypt_state(conn: &mut SimpleConnection) -> Result<CryptState, DbError> {
    let rows: Vec<(i64,)> = conn
        .query("SELECT MON$CRYPT_STATE FROM MON$DATABASE", ())
        .map_err(DbError::from)?;
    let value = rows.into_iter().next().map_or(0, |(v,)| v);
    CryptState::from_raw(value)
}

fn run_ping(conn: &mut SimpleConnection) -> Result<String, DbError> {
    let rows: Vec<(String,)> = conn
        .query(
            "SELECT rdb$get_context('SYSTEM', 'ENGINE_VERSION') FROM rdb$database",
            (),
        )
        .map_err(DbError::from)?;
    Ok(rows
        .into_iter()
        .next()
        .map_or_else(|| "unknown".into(), |(v,)| v))
}

fn run_describe_primary_keys(
    conn: &mut SimpleConnection,
) -> Result<HashMap<String, Vec<String>>, DbError> {
    let rows: Vec<RsfbRow> = conn
        .query(
            "SELECT TRIM(rc.RDB$RELATION_NAME), TRIM(s.RDB$FIELD_NAME), s.RDB$FIELD_POSITION \
             FROM RDB$RELATION_CONSTRAINTS rc \
             JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = rc.RDB$INDEX_NAME \
             WHERE rc.RDB$CONSTRAINT_TYPE = 'PRIMARY KEY' \
             ORDER BY rc.RDB$RELATION_NAME, s.RDB$FIELD_POSITION",
            (),
        )
        .map_err(DbError::from)?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let table = match row.cols.first().map(|c| &c.value) {
            Some(SqlType::Text(name)) => name.clone(),
            _ => continue,
        };
        let column = match row.cols.get(1).map(|c| &c.value) {
            Some(SqlType::Text(name)) => name.clone(),
            _ => continue,
        };
        map.entry(table).or_default().push(column);
    }
    Ok(map)
}

fn run_describe_schema(conn: &mut SimpleConnection) -> Result<Schema, DbError> {
    // The cross-join leaves columns NULL for views/tables with no
    // declared fields (rare for tables, common for some virtual
    // relations); we handle the empty-columns case below.
    let rows: Vec<RsfbRow> = conn
        .query(
            "SELECT TRIM(r.RDB$RELATION_NAME), r.RDB$RELATION_TYPE, \
             TRIM(rf.RDB$FIELD_NAME), rf.RDB$FIELD_POSITION, rf.RDB$NULL_FLAG, \
             f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE, f.RDB$FIELD_LENGTH, \
             COALESCE(f.RDB$DIMENSIONS, 0), \
             CAST(COALESCE(rf.RDB$DEFAULT_SOURCE, f.RDB$DEFAULT_SOURCE) AS VARCHAR(4000)) \
             FROM RDB$RELATIONS r \
             LEFT JOIN RDB$RELATION_FIELDS rf ON rf.RDB$RELATION_NAME = r.RDB$RELATION_NAME \
             LEFT JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = rf.RDB$FIELD_SOURCE \
             WHERE COALESCE(r.RDB$SYSTEM_FLAG, 0) = 0 \
             ORDER BY r.RDB$RELATION_NAME, rf.RDB$FIELD_POSITION",
            (),
        )
        .map_err(DbError::from)?;

    let pk_map = run_describe_primary_keys(conn).unwrap_or_default();
    let mut tables: Vec<TableInfo> = Vec::new();
    for row in rows {
        let rel_name = match row.cols.first().map(|c| &c.value) {
            Some(SqlType::Text(name)) => name.clone(),
            _ => continue,
        };
        let rel_type = match row.cols.get(1).map(|c| &c.value) {
            Some(SqlType::Integer(value)) => Some(*value),
            _ => None,
        };
        let kind = match rel_type {
            Some(1) => TableKind::View,
            _ => TableKind::Table,
        };

        let needs_new = tables.last().is_none_or(|t| t.name != rel_name);
        if needs_new {
            let primary_key = pk_map.get(&rel_name).cloned().unwrap_or_default();
            tables.push(TableInfo {
                name: rel_name,
                kind,
                columns: Vec::new(),
                primary_key,
            });
        }
        let Some(current) = tables.last_mut() else {
            continue;
        };

        let col_name = match row.cols.get(2).map(|c| &c.value) {
            Some(SqlType::Text(name)) => Some(name.clone()),
            _ => None,
        };
        let position = match row.cols.get(3).map(|c| &c.value) {
            Some(SqlType::Integer(value)) => Some(i32::try_from(*value).unwrap_or(0)),
            _ => None,
        };
        let null_flag = match row.cols.get(4).map(|c| &c.value) {
            Some(SqlType::Integer(value)) => Some(*value),
            _ => None,
        };
        let field_type = match row.cols.get(5).map(|c| &c.value) {
            Some(SqlType::Integer(value)) => Some(*value),
            _ => None,
        };
        let field_sub_type = match row.cols.get(6).map(|c| &c.value) {
            Some(SqlType::Integer(value)) => *value,
            _ => 0,
        };
        let field_length = match row.cols.get(7).map(|c| &c.value) {
            Some(SqlType::Integer(value)) => *value,
            _ => 0,
        };
        let dimensions = match row.cols.get(8).map(|c| &c.value) {
            Some(SqlType::Integer(value)) => *value,
            _ => 0,
        };
        let default_expr = match row.cols.get(9).map(|c| &c.value) {
            Some(SqlType::Text(value)) => Some(strip_default_prefix(value)),
            _ => None,
        };

        if let (Some(name), Some(pos), Some(ft)) = (col_name, position, field_type) {
            let nullable = null_flag.unwrap_or(0) == 0;
            let sql_type = field_type_name(ft, field_sub_type, field_length, dimensions);
            current.columns.push(ColumnInfo {
                name,
                position: pos,
                sql_type,
                nullable,
                default_expr,
            });
        }
    }

    let procedures = run_describe_procedures(conn).unwrap_or_default();
    let triggers = run_describe_triggers(conn).unwrap_or_default();
    let generators = run_describe_generators(conn).unwrap_or_default();
    let domains = run_describe_domains(conn).unwrap_or_default();

    Ok(Schema {
        tables,
        procedures,
        triggers,
        generators,
        domains,
    })
}

fn run_describe_procedures(conn: &mut SimpleConnection) -> Result<Vec<ProcedureInfo>, DbError> {
    let rows: Vec<RsfbRow> = conn
        .query(
            "SELECT TRIM(p.RDB$PROCEDURE_NAME), \
             COALESCE(p.RDB$PROCEDURE_INPUTS, 0), \
             COALESCE(p.RDB$PROCEDURE_OUTPUTS, 0) \
             FROM RDB$PROCEDURES p \
             WHERE COALESCE(p.RDB$SYSTEM_FLAG, 0) = 0 \
             ORDER BY p.RDB$PROCEDURE_NAME",
            (),
        )
        .map_err(DbError::from)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let name = match row.cols.first().map(|c| &c.value) {
            Some(SqlType::Text(n)) => n.clone(),
            _ => continue,
        };
        let input_count = match row.cols.get(1).map(|c| &c.value) {
            Some(SqlType::Integer(v)) => i32::try_from(*v).unwrap_or(0),
            _ => 0,
        };
        let output_count = match row.cols.get(2).map(|c| &c.value) {
            Some(SqlType::Integer(v)) => i32::try_from(*v).unwrap_or(0),
            _ => 0,
        };
        out.push(ProcedureInfo {
            name,
            input_count,
            output_count,
        });
    }
    Ok(out)
}

fn run_describe_triggers(conn: &mut SimpleConnection) -> Result<Vec<TriggerInfo>, DbError> {
    let rows: Vec<RsfbRow> = conn
        .query(
            "SELECT TRIM(t.RDB$TRIGGER_NAME), \
             COALESCE(TRIM(t.RDB$RELATION_NAME), ''), \
             COALESCE(t.RDB$TRIGGER_TYPE, 0), \
             COALESCE(t.RDB$TRIGGER_INACTIVE, 0) \
             FROM RDB$TRIGGERS t \
             WHERE COALESCE(t.RDB$SYSTEM_FLAG, 0) = 0 \
             ORDER BY t.RDB$TRIGGER_NAME",
            (),
        )
        .map_err(DbError::from)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let name = match row.cols.first().map(|c| &c.value) {
            Some(SqlType::Text(n)) => n.clone(),
            _ => continue,
        };
        let relation = match row.cols.get(1).map(|c| &c.value) {
            Some(SqlType::Text(s)) => s.clone(),
            _ => String::new(),
        };
        let trigger_type = match row.cols.get(2).map(|c| &c.value) {
            Some(SqlType::Integer(v)) => *v,
            _ => 0,
        };
        let inactive = match row.cols.get(3).map(|c| &c.value) {
            Some(SqlType::Integer(v)) => *v != 0,
            _ => false,
        };
        out.push(TriggerInfo {
            name,
            relation,
            trigger_type,
            active: !inactive,
        });
    }
    Ok(out)
}

fn run_describe_generators(conn: &mut SimpleConnection) -> Result<Vec<GeneratorInfo>, DbError> {
    let rows: Vec<RsfbRow> = conn
        .query(
            "SELECT TRIM(g.RDB$GENERATOR_NAME) \
             FROM RDB$GENERATORS g \
             WHERE COALESCE(g.RDB$SYSTEM_FLAG, 0) = 0 \
             ORDER BY g.RDB$GENERATOR_NAME",
            (),
        )
        .map_err(DbError::from)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(SqlType::Text(name)) = row.cols.first().map(|c| &c.value) else {
            continue;
        };
        let name = name.clone();
        // GEN_ID's first argument must be a literal identifier, so the
        // sub-query is built per name. Double-quote and escape internal
        // quotes to survive lowercase / mixed-case generator names.
        let escaped = name.replace('"', "\"\"");
        let value_sql = format!("SELECT GEN_ID(\"{escaped}\", 0) FROM RDB$DATABASE");
        let value_rows: Result<Vec<RsfbRow>, _> = conn.query(&value_sql, ());
        let current_value = value_rows
            .ok()
            .and_then(|vrows| {
                vrows
                    .into_iter()
                    .next()
                    .and_then(|r| r.cols.into_iter().next())
                    .and_then(|col| match col.value {
                        SqlType::Integer(v) => Some(v),
                        _ => None,
                    })
            })
            .unwrap_or(0);
        out.push(GeneratorInfo {
            name,
            current_value,
        });
    }
    Ok(out)
}

fn run_describe_domains(conn: &mut SimpleConnection) -> Result<Vec<DomainInfo>, DbError> {
    // RDB$FIELDS holds both user-declared domains and the auto-named
    // backing fields generated for table columns (`RDB$<n>`). Filter the
    // auto names out so the sidebar shows only the domains a user can
    // ALTER / DROP by name.
    let rows: Vec<RsfbRow> = conn
        .query(
            "SELECT TRIM(f.RDB$FIELD_NAME), \
             f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE, f.RDB$FIELD_LENGTH, \
             COALESCE(f.RDB$NULL_FLAG, 0), COALESCE(f.RDB$DIMENSIONS, 0) \
             FROM RDB$FIELDS f \
             WHERE COALESCE(f.RDB$SYSTEM_FLAG, 0) = 0 \
             AND f.RDB$FIELD_NAME NOT STARTING WITH 'RDB$' \
             AND f.RDB$FIELD_NAME NOT STARTING WITH 'MON$' \
             AND f.RDB$FIELD_NAME NOT STARTING WITH 'SEC$' \
             ORDER BY f.RDB$FIELD_NAME",
            (),
        )
        .map_err(DbError::from)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let name = match row.cols.first().map(|c| &c.value) {
            Some(SqlType::Text(n)) => n.clone(),
            _ => continue,
        };
        let field_type = match row.cols.get(1).map(|c| &c.value) {
            Some(SqlType::Integer(v)) => *v,
            _ => continue,
        };
        let sub_type = match row.cols.get(2).map(|c| &c.value) {
            Some(SqlType::Integer(v)) => *v,
            _ => 0,
        };
        let length = match row.cols.get(3).map(|c| &c.value) {
            Some(SqlType::Integer(v)) => *v,
            _ => 0,
        };
        let null_flag = match row.cols.get(4).map(|c| &c.value) {
            Some(SqlType::Integer(v)) => *v,
            _ => 0,
        };
        let dimensions = match row.cols.get(5).map(|c| &c.value) {
            Some(SqlType::Integer(v)) => *v,
            _ => 0,
        };
        out.push(DomainInfo {
            name,
            sql_type: field_type_name(field_type, sub_type, length, dimensions),
            nullable: null_flag == 0,
        });
    }
    Ok(out)
}

/// Trims the leading `DEFAULT ` keyword that Firebird stores verbatim
/// in `RDB$DEFAULT_SOURCE`. The remaining text is the bare default
/// expression — what would go on the right-hand side of `=` in a
/// `SET col = <expr>` clause. Leading whitespace inside the keyword
/// match is tolerated to handle the lowercased / multi-space variants
/// the engine may persist.
fn strip_default_prefix(raw: &str) -> String {
    let trimmed = raw.trim();
    let upper = trimmed.to_uppercase();
    if upper.starts_with("DEFAULT") {
        let rest = trimmed[7..].trim_start();
        return rest.to_string();
    }
    trimmed.to_string()
}

fn field_type_name(field_type: i64, sub_type: i64, length: i64, dimensions: i64) -> String {
    let base: String = match field_type {
        7 => "SMALLINT".into(),
        8 => match sub_type {
            1 => "NUMERIC".into(),
            2 => "DECIMAL".into(),
            _ => "INTEGER".into(),
        },
        10 => "FLOAT".into(),
        12 => "DATE".into(),
        13 => "TIME".into(),
        14 => format!("CHAR({length})"),
        16 => match sub_type {
            1 => "NUMERIC".into(),
            2 => "DECIMAL".into(),
            _ => "BIGINT".into(),
        },
        23 => "BOOLEAN".into(),
        24 => "DECFLOAT(16)".into(),
        25 => "DECFLOAT(34)".into(),
        26 => "INT128".into(),
        27 => "DOUBLE PRECISION".into(),
        28 => "TIME WITH TIME ZONE".into(),
        29 => "TIMESTAMP WITH TIME ZONE".into(),
        35 => "TIMESTAMP".into(),
        37 => format!("VARCHAR({length})"),
        261 => match sub_type {
            1 => "BLOB SUB_TYPE TEXT".into(),
            _ => "BLOB".into(),
        },
        other => format!("UNKNOWN({other})"),
    };
    if dimensions > 0 {
        // Firebird represents N-dimensional arrays as `<element>[]` in
        // DDL (`[]` per dimension is rendered without bounds in
        // `isql`'s `SHOW TABLE`). We mirror that minimal form; full
        // bounds (`[1:10, 1:5]`) would require a join against
        // `RDB$FIELD_DIMENSIONS` and is rarely useful in the schema
        // browser. The result-table renderer surfaces ARRAY values as
        // an opaque chip — rsfbclient 0.26 does not decode them.
        let suffix: String = "[]".repeat(usize::try_from(dimensions).unwrap_or(1));
        format!("{base}{suffix}")
    } else {
        base
    }
}

/// Firebird type codes for the integer family, which is also how
/// NUMERIC and DECIMAL are stored — as a scaled integer.
const SQL_SHORT: u32 = 500;
const SQL_LONG: u32 = 496;
const SQL_INT64: u32 = 580;

/// True when the column was declared as one of the integer-family
/// types, whatever the driver ended up handing back as a value.
fn is_integer_family(raw_type: u32) -> bool {
    // The nullable flag rides in the low bit of the type code.
    matches!(raw_type & !1, SQL_SHORT | SQL_LONG | SQL_INT64)
}

/// Maps one driver column, using its declared type to tell an exact
/// fixed-point value apart from ordinary text.
///
/// Both vendored backends render NUMERIC/DECIMAL as exact decimal text
/// rather than letting Firebird round it into a double, because
/// `SqlType` has no fixed-point variant. Text arriving on a column the
/// engine declared as an integer type can only be that rendering — a
/// real CHAR/VARCHAR column reports `SQL_TEXT` or `SQL_VARYING`.
fn column_to_value(column: &rsfbclient::Column, bin: &mut BlobBin) -> ColumnValue {
    if is_integer_family(column.raw_type) {
        if let SqlType::Text(s) = &column.value {
            return ColumnValue::Decimal(s.clone());
        }
    }
    sqltype_to_value(&column.value, bin)
}

fn sqltype_to_value(value: &SqlType, bin: &mut BlobBin) -> ColumnValue {
    match value {
        SqlType::Null => ColumnValue::Null,
        SqlType::Text(s) => ColumnValue::Text(s.clone()),
        SqlType::Integer(i) => ColumnValue::Integer(*i),
        SqlType::Floating(f) => ColumnValue::Float(*f),
        SqlType::Boolean(b) => ColumnValue::Bool(*b),
        SqlType::Timestamp(ts) => ColumnValue::Text(ts.to_string()),
        SqlType::Binary(bytes) => {
            let id = uuid::Uuid::new_v4().to_string();
            let peek_len = bytes.len().min(BLOB_PEEK_BYTES);
            let mut peek_hex = String::with_capacity(peek_len * 2);
            for byte in &bytes[..peek_len] {
                let _ = write!(peek_hex, "{byte:02x}");
            }
            let size_bytes = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
            bin.insert(id.clone(), bytes.clone());
            ColumnValue::Blob(BlobRef {
                id,
                size_bytes,
                peek_hex,
            })
        }
    }
}

/// Reads a row's column at `idx` as an i64. NULL and missing columns
/// fall back to `0`; values that are not integers fall back to `0`.
fn row_int(row: &RsfbRow, idx: usize) -> i64 {
    match row.cols.get(idx).map(|c| &c.value) {
        Some(SqlType::Integer(v)) => *v,
        _ => 0,
    }
}

/// Reads a row's column at `idx` as an owned String, trimmed of CHAR
/// padding. NULL / non-text values become an empty string.
fn row_text(row: &RsfbRow, idx: usize) -> String {
    match row.cols.get(idx).map(|c| &c.value) {
        Some(SqlType::Text(s)) => s.trim().to_string(),
        Some(SqlType::Timestamp(ts)) => ts.to_string(),
        _ => String::new(),
    }
}

fn run_database_stats(conn: &mut SimpleConnection) -> Result<DatabaseStats, DbError> {
    let database = run_mon_database(conn)?;
    let attachments = run_mon_attachments(conn).unwrap_or_default();
    let statements = run_mon_statements(conn).unwrap_or_default();
    Ok(DatabaseStats {
        database,
        attachments,
        statements,
    })
}

fn run_mon_database(conn: &mut SimpleConnection) -> Result<MonDatabase, DbError> {
    let rows: Vec<RsfbRow> = conn
        .query(
            "SELECT \
             TRIM(d.MON$DATABASE_NAME), \
             d.MON$PAGE_SIZE, d.MON$PAGES, \
             d.MON$OLDEST_TRANSACTION, d.MON$OLDEST_ACTIVE, d.MON$OLDEST_SNAPSHOT, \
             d.MON$NEXT_TRANSACTION, d.MON$SWEEP_INTERVAL, \
             d.MON$FORCED_WRITES, d.MON$READ_ONLY, d.MON$SQL_DIALECT, \
             CAST(d.MON$CREATION_DATE AS VARCHAR(64)), \
             COALESCE(d.MON$BACKUP_STATE, 0), \
             COALESCE(d.MON$SHUTDOWN_MODE, 0), \
             COALESCE(TRIM(d.MON$OWNER), ''), \
             d.MON$ODS_MAJOR, d.MON$ODS_MINOR, \
             CAST(rdb$get_context('SYSTEM', 'ENGINE_VERSION') AS VARCHAR(64)), \
             COALESCE(i.MON$PAGE_READS, 0), COALESCE(i.MON$PAGE_WRITES, 0), \
             COALESCE(i.MON$PAGE_FETCHES, 0), COALESCE(i.MON$PAGE_MARKS, 0) \
             FROM MON$DATABASE d \
             LEFT JOIN MON$IO_STATS i ON i.MON$STAT_ID = d.MON$STAT_ID",
            (),
        )
        .map_err(DbError::from)?;
    let row = rows
        .into_iter()
        .next()
        .ok_or_else(|| DbError::Driver("MON$DATABASE returned no rows".into()))?;
    Ok(MonDatabase {
        name: row_text(&row, 0),
        page_size: row_int(&row, 1),
        pages: row_int(&row, 2),
        oldest_transaction: row_int(&row, 3),
        oldest_active: row_int(&row, 4),
        oldest_snapshot: row_int(&row, 5),
        next_transaction: row_int(&row, 6),
        sweep_interval: row_int(&row, 7),
        forced_writes: row_int(&row, 8) != 0,
        read_only: row_int(&row, 9) != 0,
        sql_dialect: row_int(&row, 10),
        creation_date: row_text(&row, 11),
        backup_state: row_int(&row, 12),
        shutdown_mode: row_int(&row, 13),
        owner: row_text(&row, 14),
        ods_major: row_int(&row, 15),
        ods_minor: row_int(&row, 16),
        server_version: row_text(&row, 17),
        reads: row_int(&row, 18),
        writes: row_int(&row, 19),
        fetches: row_int(&row, 20),
        marks: row_int(&row, 21),
    })
}

fn run_mon_attachments(conn: &mut SimpleConnection) -> Result<Vec<AttachmentInfo>, DbError> {
    let rows: Vec<RsfbRow> = conn
        .query(
            "SELECT \
             a.MON$ATTACHMENT_ID, \
             COALESCE(TRIM(a.MON$USER), ''), \
             COALESCE(TRIM(a.MON$ROLE), ''), \
             COALESCE(TRIM(a.MON$REMOTE_ADDRESS), ''), \
             COALESCE(TRIM(a.MON$REMOTE_PROCESS), ''), \
             COALESCE(TRIM(c.RDB$CHARACTER_SET_NAME), ''), \
             COALESCE(a.MON$STATE, 0), \
             CAST(a.MON$TIMESTAMP AS VARCHAR(64)), \
             CASE WHEN a.MON$ATTACHMENT_ID = CURRENT_CONNECTION THEN 1 ELSE 0 END \
             FROM MON$ATTACHMENTS a \
             LEFT JOIN RDB$CHARACTER_SETS c \
             ON c.RDB$CHARACTER_SET_ID = a.MON$CHARACTER_SET_ID \
             ORDER BY a.MON$ATTACHMENT_ID",
            (),
        )
        .map_err(DbError::from)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(AttachmentInfo {
            id: row_int(&row, 0),
            user: row_text(&row, 1),
            role: row_text(&row, 2),
            remote_address: row_text(&row, 3),
            remote_process: row_text(&row, 4),
            character_set: row_text(&row, 5),
            state: row_int(&row, 6),
            timestamp: row_text(&row, 7),
            is_self: row_int(&row, 8) == 1,
        });
    }
    Ok(out)
}

fn run_mon_statements(conn: &mut SimpleConnection) -> Result<Vec<StatementInfo>, DbError> {
    let rows: Vec<RsfbRow> = conn
        .query(
            "SELECT \
             s.MON$STATEMENT_ID, s.MON$ATTACHMENT_ID, \
             COALESCE(s.MON$STATE, 0), \
             CAST(s.MON$TIMESTAMP AS VARCHAR(64)), \
             COALESCE(s.MON$SQL_TEXT, '') \
             FROM MON$STATEMENTS s \
             ORDER BY s.MON$ATTACHMENT_ID, s.MON$STATEMENT_ID",
            (),
        )
        .map_err(DbError::from)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(StatementInfo {
            id: row_int(&row, 0),
            attachment_id: row_int(&row, 1),
            state: row_int(&row, 2),
            timestamp: row_text(&row, 3),
            sql_text: row_text(&row, 4),
        });
    }
    Ok(out)
}
