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

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use plamenix_types::{ConnectionConfig, SessionId};
use rsfbclient::{Execute, Queryable, Row as RsfbRow, SimpleConnection, SqlType};
use tokio::sync::Mutex;

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
        let conn = tokio::task::spawn_blocking(move || build_connection(&config, &mode)).await??;

        let id = SessionId::new();
        let shared = Arc::new(Mutex::new(conn));

        self.sessions.lock().await.insert(id, shared);
        tracing::info!(?id, "session attached");
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
    let mut builder = rsfbclient::builder_native();
    if let Some(path) = &config.fbclient_path {
        builder.with_dyn_load(path);
    } else {
        builder.with_dyn_link();
    }
    builder
        .host(&config.host)
        .port(config.port)
        .db_name(&config.database)
        .user(&config.user)
        .pass(&config.password)
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
