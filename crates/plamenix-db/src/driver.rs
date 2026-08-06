//! The [`DbDriver`] trait — one of Plamenix's three swap-point traits.
//!
//! Concrete implementations live under [`crate::rsfb`]. The trait stays
//! deliberately small: connect, execute, ping, close. Anything richer
//! (transactions, prepared statements, streaming cursors) lands when a
//! caller actually needs it.

use async_trait::async_trait;
use plamenix_types::{ConnectionConfig, DatabaseStats, Schema, SessionId};

use crate::crypt::CryptState;
use crate::error::DbError;
use crate::query::QueryResult;
use crate::transaction::{TxConfig, TxMode, TxStatus};

/// Which backend a [`DbDriver`] should use.
///
/// Default is `Native` with a user-supplied `fbclient` path (the bundled
/// library). `PureRust` skips fbclient entirely and speaks the wire
/// protocol directly; see `../../docs/firebird-driver.md` for the
/// feature-coverage trade-offs.
#[derive(Clone, Debug, Default)]
pub enum ConnectMode {
    /// Native fbclient loaded at runtime from a path. Default mode.
    #[default]
    Native,
    /// Pure-Rust wire-protocol implementation; no fbclient on disk.
    PureRust,
}

/// The async, dyn-compatible Firebird driver trait.
///
/// Implementations are expected to be cheap to clone (typically an
/// `Arc` wrapper around inner state) so they can be parked in per-tab
/// registries without lifetime juggling.
///
/// # Concurrency
///
/// The driver is dyn-compatible so consumers can store
/// `Arc<dyn DbDriver>`. Concurrent calls against the same `SessionId`
/// are serialised by the implementation (one in-flight call per
/// session), mirroring the MVP's per-attachment lock and preventing
/// driver-level deadlocks in rsfbclient.
#[async_trait]
pub trait DbDriver: Send + Sync {
    /// Attaches to a Firebird database and returns a fresh session id.
    async fn connect(
        &self,
        config: ConnectionConfig,
        mode: ConnectMode,
    ) -> Result<SessionId, DbError>;

    /// Runs `sql` on `session` and returns the result.
    async fn execute(&self, session: SessionId, sql: String) -> Result<QueryResult, DbError>;

    /// Lightweight liveness check: returns the server version, or an
    /// error if the attachment is dead. Useful for connection-pool
    /// resurrection and for the UI's status badge.
    async fn ping(&self, session: SessionId) -> Result<String, DbError>;

    /// Detaches the session. After this returns, the `SessionId` is no
    /// longer valid for any call on this driver.
    async fn close(&self, session: SessionId) -> Result<(), DbError>;

    /// Reads `MON$DATABASE.MON$CRYPT_STATE` and returns the typed
    /// [`CryptState`]. Used by the UI status badge and by the
    /// `encryption_required` check inside [`Self::connect`].
    async fn crypt_state(&self, session: SessionId) -> Result<CryptState, DbError>;

    /// Returns the catalogue of tables and views visible to `session`.
    ///
    /// Filters out system relations via `RDB$SYSTEM_FLAG`; persistent
    /// tables and views are both returned, distinguished by
    /// [`plamenix_types::TableKind`].
    async fn describe_schema(&self, session: SessionId) -> Result<Schema, DbError>;

    /// Returns a snapshot of `MON$DATABASE`, `MON$ATTACHMENTS`, and
    /// `MON$STATEMENTS` for the dashboard. Cheap to call repeatedly
    /// (the UI polls on demand), but it does hit the engine — the
    /// caller is responsible for cadence.
    async fn database_stats(&self, session: SessionId) -> Result<DatabaseStats, DbError>;

    /// Returns the cached BLOB body for `(session, blob_id)`.
    ///
    /// BLOB bodies are populated during `execute` and held until the
    /// next execute on the same session (or until the session closes).
    /// Returns an error when the cache no longer contains the id —
    /// typically because the user has re-run the query.
    async fn fetch_blob(&self, session: SessionId, blob_id: String) -> Result<Vec<u8>, DbError>;

    /// Switches a session between autocommit and manual commit, and
    /// sets how its explicit transactions are started.
    ///
    /// # Errors
    ///
    /// Refuses the switch while a transaction is open — the caller must
    /// commit or roll back first, so no work is silently discarded or
    /// silently committed by a mode change.
    async fn set_transaction_mode(
        &self,
        session: SessionId,
        mode: TxMode,
        config: TxConfig,
    ) -> Result<TxStatus, DbError>;

    /// Opens an explicit transaction.
    ///
    /// Callers rarely need this: in manual mode the first statement
    /// opens one, matching what every SQL client does. It exists so a
    /// user can start a transaction deliberately — to hold a snapshot
    /// before reading, say — and so tests can be explicit.
    ///
    /// # Errors
    ///
    /// Fails when one is already open, or when the session is in
    /// autocommit.
    async fn begin_transaction(&self, session: SessionId) -> Result<TxStatus, DbError>;

    /// Commits the open transaction.
    ///
    /// # Errors
    ///
    /// Fails when no transaction is open.
    async fn commit(&self, session: SessionId) -> Result<TxStatus, DbError>;

    /// Rolls back the open transaction, discarding every statement run
    /// since it opened — DDL included, which Firebird makes
    /// transactional.
    ///
    /// # Errors
    ///
    /// Fails when no transaction is open.
    async fn rollback(&self, session: SessionId) -> Result<TxStatus, DbError>;

    /// Current transaction state, for the UI indicator.
    ///
    /// Cheap: answered from driver state without touching the engine,
    /// so it is safe to poll for the age readout.
    async fn transaction_status(&self, session: SessionId) -> Result<TxStatus, DbError>;
}
