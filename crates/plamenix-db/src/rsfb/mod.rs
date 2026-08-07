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
use std::time::Instant;

use async_trait::async_trait;
use plamenix_types::{
    AttachmentInfo, ColumnInfo, ConnectionConfig, DatabaseStats, DomainInfo, GeneratorInfo,
    MonDatabase, ProcedureInfo, Schema, SessionId, StatementInfo, TableInfo, TableKind,
    TriggerInfo,
};
use rsfbclient::prelude::{
    TrDataAccessMode, TrIsolationLevel, TrLockResolution, TrRecordVersion, TransactionConfiguration,
};
use rsfbclient::{Execute, Queryable, Row as RsfbRow, SimpleConnection, SqlType};
use tokio::sync::Mutex;

use crate::crypt::CryptState;
use crate::driver::{ConnectMode, DbDriver};
use crate::error::DbError;
use crate::query::{BlobRef, Column, ColumnValue, QueryResult, Row};
use crate::transaction::{TxConfig, TxIsolation, TxLocking, TxMode, TxStatus};

type SharedConn = Arc<Mutex<SimpleConnection>>;

/// Runtime transaction state for one session.
#[derive(Clone, Copy, Debug)]
struct TxRuntime {
    mode: TxMode,
    config: TxConfig,
    open: bool,
    pending: u32,
    started_at: Option<Instant>,
}

impl TxRuntime {
    const fn new() -> Self {
        Self {
            mode: TxMode::Autocommit,
            config: TxConfig {
                isolation: TxIsolation::ReadCommitted,
                locking: TxLocking::NoWait,
            },
            open: false,
            pending: 0,
            started_at: None,
        }
    }

    fn status(&self) -> TxStatus {
        TxStatus {
            mode: self.mode,
            config: self.config,
            open: self.open,
            pending_statements: self.pending,
            age_ms: self
                .started_at
                .map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX))
                .unwrap_or(0),
        }
    }
}

/// One attached session.
///
/// Two attachments, deliberately. `work` runs the statements the user
/// types and is what manual mode holds a transaction on. `meta` serves
/// Plamenix's own reads — schema, dashboard, ping, crypt state — as a
/// read-only read-committed attachment, so background chatter never
/// joins the user's transaction and browsing never holds anything open.
///
/// The user's own statements stay on `work` whatever they are, so a
/// `SELECT` after an `UPDATE` sees the uncommitted change, as it must.
#[derive(Clone)]
struct Session {
    work: SharedConn,
    meta: SharedConn,
    tx: Arc<Mutex<TxRuntime>>,
    /// Engine major version, probed once at attach.
    ///
    /// Plamenix supports Firebird 2.5 through 5.0 and two monitoring
    /// columns only exist from 3.0 onward, so a query built for the
    /// newest engine fails outright on the oldest. Probed rather than
    /// inferred: the client library version says nothing about what the
    /// server it connected to supports.
    engine_major: u32,
}

/// Transaction settings for the metadata attachment.
///
/// Read-only so it can never write, and read-committed so it never
/// holds a snapshot: metadata reads must not pin the oldest active
/// transaction while the user browses.
fn meta_tx_config() -> TransactionConfiguration {
    TransactionConfiguration {
        data_access: TrDataAccessMode::ReadOnly,
        isolation: TrIsolationLevel::ReadCommited(TrRecordVersion::NoRecordVersion),
        lock_resolution: TrLockResolution::NoWait,
    }
}

/// Maps Plamenix's transaction settings onto rsfbclient's.
fn work_tx_config(config: TxConfig) -> TransactionConfiguration {
    TransactionConfiguration {
        data_access: TrDataAccessMode::ReadWrite,
        isolation: match config.isolation {
            TxIsolation::ReadCommitted => {
                TrIsolationLevel::ReadCommited(TrRecordVersion::RecordVersion)
            }
            TxIsolation::Snapshot => TrIsolationLevel::Concurrency,
        },
        lock_resolution: match config.locking {
            TxLocking::Wait(timeout) => TrLockResolution::Wait(timeout),
            TxLocking::NoWait => TrLockResolution::NoWait,
        },
    }
}

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
    sessions: Arc<Mutex<HashMap<SessionId, Session>>>,
    blobs: Arc<Mutex<HashMap<SessionId, BlobBin>>>,
}

impl RsfbDriver {
    /// Returns a new, empty driver.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    async fn session(&self, session: SessionId) -> Result<Session, DbError> {
        let sessions = self.sessions.lock().await;
        sessions
            .get(&session)
            .cloned()
            .ok_or_else(|| DbError::Driver(format!("unknown session: {session:?}")))
    }

    /// The read-only attachment used for schema, dashboard and ping, so
    /// they neither join the user's transaction nor hold one open.
    async fn meta_conn(&self, session: SessionId) -> Result<SharedConn, DbError> {
        Ok(self.session(session).await?.meta)
    }

    /// Ends the open transaction, committing or rolling it back.
    ///
    /// State is cleared whatever the engine says. If the commit itself
    /// failed the transaction is gone regardless, and leaving the
    /// session believing it still holds one would strand it — every
    /// later statement would try to join a transaction that no longer
    /// exists.
    async fn finish_transaction(
        &self,
        session: SessionId,
        commit: bool,
    ) -> Result<TxStatus, DbError> {
        let state = self.session(session).await?;
        let mut tx = state.tx.lock().await;
        if !tx.open {
            return Err(DbError::Driver("no transaction is open".into()));
        }
        let conn = Arc::clone(&state.work);
        let outcome = tokio::task::spawn_blocking(move || {
            let mut guard = conn.blocking_lock();
            if commit {
                guard.commit()
            } else {
                guard.rollback()
            }
            .map_err(|err| DbError::Driver(err.to_string()))
        })
        .await?;

        tx.open = false;
        tx.pending = 0;
        tx.started_at = None;
        outcome?;
        Ok(tx.status())
    }

    /// Applies a mutation to a session's transaction state and returns
    /// the resulting status.
    async fn with_tx<F>(&self, session: SessionId, f: F) -> Result<TxStatus, DbError>
    where
        F: FnOnce(&mut TxRuntime) -> Result<(), DbError>,
    {
        let state = self.session(session).await?;
        let mut tx = state.tx.lock().await;
        f(&mut tx)?;
        Ok(tx.status())
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

        // Two attachments per session: one for the user's statements,
        // one read-only for Plamenix's own metadata reads. Opening both
        // up front keeps the read path available even while the work
        // attachment sits inside a long manual transaction.
        let (conn, meta, engine_major) = {
            let config = config.clone();
            let mode = mode.clone();
            tokio::task::spawn_blocking(move || -> Result<_, DbError> {
                let work = build_connection(
                    &config,
                    &mode,
                    work_tx_config(TxConfig {
                        isolation: TxIsolation::ReadCommitted,
                        locking: TxLocking::NoWait,
                    }),
                )?;
                let mut meta = build_connection(&config, &mode, meta_tx_config())?;
                // Probed on the metadata attachment, before any user
                // statement can be in flight.
                let engine_major = probe_engine_major(&mut meta);
                Ok((work, meta, engine_major))
            })
            .await??
        };

        // Best-effort wipe of the process-global key slot once the
        // attach handshake completes (success or failure). Leaves
        // memory pressure lower than holding the key for the
        // remainder of the process lifetime.
        #[cfg(feature = "native")]
        crate::crypt_callback::clear_key();

        let id = SessionId::new();
        let state = Session {
            work: Arc::new(Mutex::new(conn)),
            meta: Arc::new(Mutex::new(meta)),
            tx: Arc::new(Mutex::new(TxRuntime::new())),
            engine_major,
        };

        self.sessions.lock().await.insert(id, state);
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
        // A typed COMMIT or ROLLBACK is transaction control, not a
        // statement to hand the engine — rsfbclient owns the transaction
        // handle, so passing it through would fail. Route it to the same
        // path the toolbar buttons use.
        match transaction_keyword(&sql) {
            Some(TransactionKeyword::Commit) => {
                self.commit(session).await?;
                return Ok(QueryResult::Affected { rows: 0 });
            }
            Some(TransactionKeyword::Rollback) => {
                self.rollback(session).await?;
                return Ok(QueryResult::Affected { rows: 0 });
            }
            None => {}
        }

        let state = self.session(session).await?;
        // In manual mode the first statement opens the transaction, the
        // way every SQL client behaves — the user should not have to
        // press "begin" before typing.
        {
            let mut tx = state.tx.lock().await;
            if tx.mode == TxMode::Manual && !tx.open {
                let conn = Arc::clone(&state.work);
                let confs = work_tx_config(tx.config);
                tokio::task::spawn_blocking(move || {
                    conn.blocking_lock()
                        .begin_transaction_config(confs)
                        .map_err(|err| DbError::Driver(err.to_string()))
                })
                .await??;
                tx.open = true;
                tx.pending = 0;
                tx.started_at = Some(Instant::now());
            }
        }

        let shared = Arc::clone(&state.work);
        let blobs_arc = Arc::clone(&self.blobs);
        let (result, bin) =
            tokio::task::spawn_blocking(move || -> Result<(QueryResult, BlobBin), DbError> {
                let mut guard = shared.blocking_lock();
                let mut bin: BlobBin = HashMap::new();
                let result = run_statement(&mut guard, &sql, &mut bin)?;
                Ok((result, bin))
            })
            .await??;

        // Counted only on success. A failed statement leaves the
        // transaction open and usable — Firebird does not abort it — so
        // the user can correct the statement and carry on, but it did
        // not contribute anything a rollback would discard.
        {
            let mut tx = state.tx.lock().await;
            if tx.open {
                tx.pending = tx.pending.saturating_add(1);
            }
        }
        // Replace any previous BLOB cache for this session — fresh
        // execute, fresh handles.
        blobs_arc.lock().await.insert(session, bin);
        Ok(result)
    }

    #[tracing::instrument(name = "db.ping", skip(self), fields(session = %session.0))]
    async fn ping(&self, session: SessionId) -> Result<String, DbError> {
        let shared = self.meta_conn(session).await?;
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.blocking_lock();
            run_ping(&mut guard)
        })
        .await?
    }

    #[tracing::instrument(name = "db.close", skip(self), fields(session = %session.0))]
    async fn close(&self, session: SessionId) -> Result<(), DbError> {
        let mut sessions = self.sessions.lock().await;
        let Some(state) = sessions.remove(&session) else {
            return Err(DbError::Driver(format!("unknown session: {session:?}")));
        };
        drop(sessions);

        // Roll back rather than commit. Detaching is not consent to
        // write: a session going away with work outstanding — a closed
        // tab, a dropped connection — must not silently commit it. The
        // shell is expected to have asked the user first; this is the
        // backstop for the paths that cannot ask.
        let mut tx = state.tx.lock().await;
        if tx.open {
            let conn = Arc::clone(&state.work);
            let pending = tx.pending;
            let rolled = tokio::task::spawn_blocking(move || {
                conn.blocking_lock()
                    .rollback()
                    .map_err(|err| DbError::Driver(err.to_string()))
            })
            .await?;
            match rolled {
                Ok(()) => tracing::warn!(
                    ?session,
                    pending,
                    "session closed with an open transaction; rolled back",
                ),
                Err(err) => tracing::error!(
                    ?session,
                    pending,
                    ?err,
                    "session closed with an open transaction and the rollback failed",
                ),
            }
            tx.open = false;
            tx.pending = 0;
            tx.started_at = None;
        }
        drop(tx);

        self.blobs.lock().await.remove(&session);
        tracing::info!(?session, "session detached");
        Ok(())
    }

    #[tracing::instrument(name = "db.set_transaction_mode", skip(self), fields(session = %session.0, ?mode))]
    async fn set_transaction_mode(
        &self,
        session: SessionId,
        mode: TxMode,
        config: TxConfig,
    ) -> Result<TxStatus, DbError> {
        self.with_tx(session, |tx| {
            if tx.open {
                return Err(DbError::Driver(
                    "commit or roll back before changing transaction mode".into(),
                ));
            }
            tx.mode = mode;
            tx.config = config;
            Ok(())
        })
        .await
    }

    #[tracing::instrument(name = "db.begin_transaction", skip(self), fields(session = %session.0))]
    async fn begin_transaction(&self, session: SessionId) -> Result<TxStatus, DbError> {
        let state = self.session(session).await?;
        let mut tx = state.tx.lock().await;
        if tx.open {
            return Err(DbError::Driver("a transaction is already open".into()));
        }
        if tx.mode != TxMode::Manual {
            return Err(DbError::Driver(
                "switch to manual commit before opening a transaction".into(),
            ));
        }
        let conn = Arc::clone(&state.work);
        let confs = work_tx_config(tx.config);
        tokio::task::spawn_blocking(move || {
            conn.blocking_lock()
                .begin_transaction_config(confs)
                .map_err(|err| DbError::Driver(err.to_string()))
        })
        .await??;
        tx.open = true;
        tx.pending = 0;
        tx.started_at = Some(Instant::now());
        Ok(tx.status())
    }

    #[tracing::instrument(name = "db.commit", skip(self), fields(session = %session.0))]
    async fn commit(&self, session: SessionId) -> Result<TxStatus, DbError> {
        self.finish_transaction(session, true).await
    }

    #[tracing::instrument(name = "db.rollback", skip(self), fields(session = %session.0))]
    async fn rollback(&self, session: SessionId) -> Result<TxStatus, DbError> {
        self.finish_transaction(session, false).await
    }

    async fn transaction_status(&self, session: SessionId) -> Result<TxStatus, DbError> {
        let state = self.session(session).await?;
        let tx = state.tx.lock().await;
        Ok(tx.status())
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
        let state = self.session(session).await?;
        let engine_major = state.engine_major;
        let shared = state.meta;
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.blocking_lock();
            run_crypt_state(&mut guard, engine_major)
        })
        .await?
    }

    #[tracing::instrument(
        name = "db.describe_schema",
        skip(self),
        fields(session = %session.0),
    )]
    async fn describe_schema(&self, session: SessionId) -> Result<Schema, DbError> {
        let shared = self.meta_conn(session).await?;
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
        let state = self.session(session).await?;
        let engine_major = state.engine_major;
        let shared = state.meta;
        tokio::task::spawn_blocking(move || {
            let mut guard = shared.blocking_lock();
            run_database_stats(&mut guard, engine_major)
        })
        .await?
    }
}

fn build_connection(
    config: &ConnectionConfig,
    mode: &ConnectMode,
    tx: TransactionConfiguration,
) -> Result<SimpleConnection, DbError> {
    match mode {
        ConnectMode::Native => build_native(config, tx),
        ConnectMode::PureRust => build_pure_rust(config, tx),
    }
}

/// Creates an embedded Firebird database file, then drops the
/// connection that made it.
///
/// Attaching and creating are different operations in Firebird, and
/// `connect` only attaches — a caller that owns its own local database
/// (Plamenix's metadata store) has to be able to bring one into
/// existence on first run rather than shipping an empty `.fdb` or
/// asking the user to run `isql`.
///
/// Idempotent by omission: it is the caller's job to check whether the
/// file exists first, because "create" on an existing Firebird database
/// is an error and not a benign one.
///
/// # Errors
///
/// [`DbError::Connect`] when the engine refuses, including when the
/// file already exists.
#[cfg(feature = "native")]
pub fn create_embedded_database(config: &ConnectionConfig) -> Result<(), DbError> {
    let Some(path) = resolver::resolve_fbclient_path(config) else {
        return Err(DbError::Connect(
            "creating an embedded database requires a bundled fbclient".into(),
        ));
    };
    let mut builder = rsfbclient::builder_native()
        .with_dyn_load(path.to_string_lossy().into_owned())
        .with_embedded();
    builder.db_name(&config.database);
    builder.user(&config.user);
    apply_charset_embedded(&mut builder, config)?;
    builder
        .create_database()
        .map(|_| ())
        .map_err(|err| DbError::Connect(err.to_string()))
}

#[cfg(feature = "native")]
fn build_native(
    config: &ConnectionConfig,
    tx: TransactionConfiguration,
) -> Result<SimpleConnection, DbError> {
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
        builder.transaction(tx);
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
    builder.transaction(tx);
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
fn build_native(
    _config: &ConnectionConfig,
    _tx: TransactionConfiguration,
) -> Result<SimpleConnection, DbError> {
    Err(DbError::Driver(
        "native backend not compiled in (enable the `native` feature)".into(),
    ))
}

#[cfg(feature = "pure-rust")]
fn build_pure_rust(
    config: &ConnectionConfig,
    tx: TransactionConfiguration,
) -> Result<SimpleConnection, DbError> {
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
    builder.transaction(tx);
    builder
        .connect()
        .map(SimpleConnection::from)
        .map_err(|err| DbError::Connect(err.to_string()))
}

#[cfg(not(feature = "pure-rust"))]
fn build_pure_rust(
    _config: &ConnectionConfig,
    _tx: TransactionConfiguration,
) -> Result<SimpleConnection, DbError> {
    Err(DbError::Driver(
        "pure-rust backend not compiled in (enable the `pure-rust` feature)".into(),
    ))
}

/// A statement that is transaction control rather than SQL to execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransactionKeyword {
    Commit,
    Rollback,
}

/// Recognises a bare `COMMIT` or `ROLLBACK`.
///
/// Only the bare forms. `COMMIT RETAINING` is deliberately not matched:
/// it commits while keeping the transaction context, so the oldest
/// active transaction stays pinned and garbage collection stays
/// stalled — the behaviour this feature exists to avoid. `ROLLBACK TO
/// SAVEPOINT` is likewise unmatched, since savepoints are out of scope;
/// both fall through to the engine, which reports them plainly.
fn transaction_keyword(sql: &str) -> Option<TransactionKeyword> {
    let normalised = sql.trim().trim_end_matches(';');
    let mut words = normalised.split_whitespace();
    let first = words.next()?.to_ascii_uppercase();
    // A trailing WORK is the SQL-standard noise word and changes nothing.
    let rest: Vec<String> = words.map(|w| w.to_ascii_uppercase()).collect();
    let bare = rest.is_empty() || rest == ["WORK"];
    if !bare {
        return None;
    }
    match first.as_str() {
        "COMMIT" => Some(TransactionKeyword::Commit),
        "ROLLBACK" => Some(TransactionKeyword::Rollback),
        _ => None,
    }
}

fn run_statement(
    conn: &mut SimpleConnection,
    sql: &str,
    bin: &mut BlobBin,
) -> Result<QueryResult, DbError> {
    // Routing shares `statement_shape` with the command layer. There used
    // to be a second, narrower copy of the test here that omitted
    // EXECUTE entirely, so an EXECUTE BLOCK went down the non-cursor
    // path and its rows were replaced by an affected-row count. Real
    // prepared-statement metadata would beat any keyword test; until
    // that plumbing lands, one classifier beats two that disagree.
    let rows: Vec<RsfbRow> = match crate::statement_shape(sql) {
        crate::StatementShape::NoResultSet => {
            let affected = conn.execute(sql, ()).map_err(DbError::from)?;
            return Ok(QueryResult::Affected {
                rows: u64::try_from(affected).unwrap_or(0),
            });
        }
        crate::StatementShape::OutputParams => {
            // EXECUTE PROCEDURE returns its output parameters as a
            // single row through `isc_dsql_execute2`, with no cursor to
            // fetch — asking for one fails with "Cursor is not open".
            // `Row` implements `FromRow`, so the columns stay dynamic.
            let row: RsfbRow = conn.execute_returnable(sql, ()).map_err(DbError::from)?;
            vec![row]
        }
        crate::StatementShape::Cursor => conn.query(sql, ()).map_err(DbError::from)?,
    };
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

fn run_crypt_state(conn: &mut SimpleConnection, engine_major: u32) -> Result<CryptState, DbError> {
    // MON$CRYPT_STATE arrived with Firebird 3.0, alongside database
    // encryption itself. On 2.5 the column is not merely absent — the
    // engine has no native encryption at all, so unencrypted is the
    // factual answer rather than a fallback, and `encryption_required`
    // correctly refuses such a connection.
    if engine_major < 3 {
        return Ok(CryptState::Unencrypted);
    }
    let rows: Vec<(i64,)> = conn
        .query("SELECT MON$CRYPT_STATE FROM MON$DATABASE", ())
        .map_err(DbError::from)?;
    let value = rows.into_iter().next().map_or(0, |(v,)| v);
    CryptState::from_raw(value)
}

/// Reads the engine's major version.
///
/// Falls back to 0 rather than failing the attach: an unreadable
/// version should degrade to the conservative query shape, not stop the
/// user connecting.
fn probe_engine_major(conn: &mut SimpleConnection) -> u32 {
    run_ping(conn).map_or(0, |version| {
        version
            .split('.')
            .next()
            .and_then(|major| major.parse().ok())
            .unwrap_or(0)
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

fn run_database_stats(
    conn: &mut SimpleConnection,
    engine_major: u32,
) -> Result<DatabaseStats, DbError> {
    let database = run_mon_database(conn, engine_major)?;
    let attachments = run_mon_attachments(conn).unwrap_or_default();
    let statements = run_mon_statements(conn).unwrap_or_default();
    Ok(DatabaseStats {
        database,
        attachments,
        statements,
    })
}

fn run_mon_database(
    conn: &mut SimpleConnection,
    engine_major: u32,
) -> Result<MonDatabase, DbError> {
    // MON$OWNER arrived with Firebird 3.0. Selecting it on 2.5 fails the
    // whole query with "Column unknown", taking the entire dashboard
    // down rather than one field, so the column is substituted out.
    let owner = if engine_major >= 3 {
        "COALESCE(TRIM(d.MON$OWNER), '')"
    } else {
        "CAST('' AS VARCHAR(64))"
    };
    let rows: Vec<RsfbRow> = conn
        .query(
            &format!(
                "SELECT \
             TRIM(d.MON$DATABASE_NAME), \
             d.MON$PAGE_SIZE, d.MON$PAGES, \
             d.MON$OLDEST_TRANSACTION, d.MON$OLDEST_ACTIVE, d.MON$OLDEST_SNAPSHOT, \
             d.MON$NEXT_TRANSACTION, d.MON$SWEEP_INTERVAL, \
             d.MON$FORCED_WRITES, d.MON$READ_ONLY, d.MON$SQL_DIALECT, \
             CAST(d.MON$CREATION_DATE AS VARCHAR(64)), \
             COALESCE(d.MON$BACKUP_STATE, 0), \
             COALESCE(d.MON$SHUTDOWN_MODE, 0), \
             {owner}, \
             d.MON$ODS_MAJOR, d.MON$ODS_MINOR, \
             CAST(rdb$get_context('SYSTEM', 'ENGINE_VERSION') AS VARCHAR(64)), \
             COALESCE(i.MON$PAGE_READS, 0), COALESCE(i.MON$PAGE_WRITES, 0), \
             COALESCE(i.MON$PAGE_FETCHES, 0), COALESCE(i.MON$PAGE_MARKS, 0) \
             FROM MON$DATABASE d \
             LEFT JOIN MON$IO_STATS i ON i.MON$STAT_ID = d.MON$STAT_ID"
            ),
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
