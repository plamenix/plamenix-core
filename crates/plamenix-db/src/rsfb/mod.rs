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
use plamenix_types::{ColumnInfo, ConnectionConfig, Schema, SessionId, TableInfo, TableKind};
use rsfbclient::{Execute, Queryable, Row as RsfbRow, SimpleConnection, SqlType};
use tokio::sync::Mutex;

use crate::crypt::CryptState;
use crate::driver::{ConnectMode, DbDriver};
use crate::error::DbError;
use crate::query::{Column, ColumnValue, QueryResult, Row};

type SharedConn = Arc<Mutex<SimpleConnection>>;

/// The default Plamenix driver. Cheap to clone; cloning shares the
/// session registry through an `Arc`.
#[derive(Clone, Default)]
pub struct RsfbDriver {
    sessions: Arc<Mutex<HashMap<SessionId, SharedConn>>>,
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
    async fn connect(
        &self,
        config: ConnectionConfig,
        mode: ConnectMode,
    ) -> Result<SessionId, DbError> {
        let encryption_required = config.encryption_required;
        let conn = tokio::task::spawn_blocking(move || build_connection(&config, &mode)).await??;

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

    async fn execute(&self, session: SessionId, sql: String) -> Result<QueryResult, DbError> {
        let shared = self.shared_conn(session).await?;
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.blocking_lock();
            run_statement(&mut guard, &sql)
        })
        .await?
    }

    async fn ping(&self, session: SessionId) -> Result<String, DbError> {
        let shared = self.shared_conn(session).await?;
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.blocking_lock();
            run_ping(&mut guard)
        })
        .await?
    }

    async fn close(&self, session: SessionId) -> Result<(), DbError> {
        let mut sessions = self.sessions.lock().await;
        if sessions.remove(&session).is_some() {
            tracing::info!(?session, "session detached");
            Ok(())
        } else {
            Err(DbError::Driver(format!("unknown session: {session:?}")))
        }
    }

    async fn crypt_state(&self, session: SessionId) -> Result<CryptState, DbError> {
        let shared = self.shared_conn(session).await?;
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.blocking_lock();
            run_crypt_state(&mut guard)
        })
        .await?
    }

    async fn describe_schema(&self, session: SessionId) -> Result<Schema, DbError> {
        let shared = self.shared_conn(session).await?;
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.blocking_lock();
            run_describe_schema(&mut guard)
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
    let mut builder = rsfbclient::builder_native()
        .with_dyn_load(path_str)
        .with_remote();
    builder
        .host(&config.host)
        .port(config.port)
        .db_name(&config.database)
        .user(&config.user)
        .pass(&config.password);
    builder
        .connect()
        .map(SimpleConnection::from)
        .map_err(|err| DbError::Connect(err.to_string()))
}

#[cfg(not(feature = "native"))]
fn build_native(_config: &ConnectionConfig) -> Result<SimpleConnection, DbError> {
    Err(DbError::Driver(
        "native backend not compiled in (enable the `native` feature)".into(),
    ))
}

#[cfg(feature = "pure-rust")]
fn build_pure_rust(config: &ConnectionConfig) -> Result<SimpleConnection, DbError> {
    rsfbclient::builder_pure_rust()
        .host(&config.host)
        .port(config.port)
        .db_name(&config.database)
        .user(&config.user)
        .pass(&config.password)
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

fn run_statement(conn: &mut SimpleConnection, sql: &str) -> Result<QueryResult, DbError> {
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
            cells: row
                .cols
                .iter()
                .map(|c| sqltype_to_value(&c.value))
                .collect(),
        })
        .collect();

    Ok(QueryResult::Rows {
        columns,
        rows: mapped_rows,
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

fn run_describe_schema(conn: &mut SimpleConnection) -> Result<Schema, DbError> {
    // The cross-join leaves columns NULL for views/tables with no
    // declared fields (rare for tables, common for some virtual
    // relations); we handle the empty-columns case below.
    let rows: Vec<RsfbRow> = conn
        .query(
            "SELECT TRIM(r.RDB$RELATION_NAME), r.RDB$RELATION_TYPE, \
             TRIM(rf.RDB$FIELD_NAME), rf.RDB$FIELD_POSITION, rf.RDB$NULL_FLAG, \
             f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE, f.RDB$FIELD_LENGTH \
             FROM RDB$RELATIONS r \
             LEFT JOIN RDB$RELATION_FIELDS rf ON rf.RDB$RELATION_NAME = r.RDB$RELATION_NAME \
             LEFT JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = rf.RDB$FIELD_SOURCE \
             WHERE COALESCE(r.RDB$SYSTEM_FLAG, 0) = 0 \
             ORDER BY r.RDB$RELATION_NAME, rf.RDB$FIELD_POSITION",
            (),
        )
        .map_err(DbError::from)?;

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
            tables.push(TableInfo {
                name: rel_name,
                kind,
                columns: Vec::new(),
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

        if let (Some(name), Some(pos), Some(ft)) = (col_name, position, field_type) {
            let nullable = null_flag.unwrap_or(0) == 0;
            let sql_type = field_type_name(ft, field_sub_type, field_length);
            current.columns.push(ColumnInfo {
                name,
                position: pos,
                sql_type,
                nullable,
            });
        }
    }

    Ok(Schema { tables })
}

fn field_type_name(field_type: i64, sub_type: i64, length: i64) -> String {
    match field_type {
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
    }
}

fn sqltype_to_value(value: &SqlType) -> ColumnValue {
    match value {
        SqlType::Null => ColumnValue::Null,
        SqlType::Text(s) => ColumnValue::Text(s.clone()),
        SqlType::Integer(i) => ColumnValue::Integer(*i),
        SqlType::Floating(f) => ColumnValue::Float(*f),
        SqlType::Boolean(b) => ColumnValue::Bool(*b),
        SqlType::Timestamp(ts) => ColumnValue::Text(ts.to_string()),
        SqlType::Binary(bytes) => {
            let mut hex = String::with_capacity(bytes.len() * 2);
            for byte in bytes {
                let _ = write!(hex, "{byte:02x}");
            }
            ColumnValue::Blob(hex)
        }
    }
}
