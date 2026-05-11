//! The [`DbDriver`] trait — one of Plamenix's three swap-point traits.
//!
//! Concrete implementations live under [`crate::rsfb`]. The trait stays
//! deliberately small: connect, execute, ping, close. Anything richer
//! (transactions, prepared statements, streaming cursors) lands when a
//! caller actually needs it.

use async_trait::async_trait;
use plamenix_types::{ConnectionConfig, Schema, SessionId};

use crate::crypt::CryptState;
use crate::error::DbError;
use crate::query::QueryResult;

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
}
