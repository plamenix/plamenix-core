//! Explicit transaction control.
//!
//! Firebird has no "outside a transaction" state — every statement runs
//! in one — so autocommit is not a mode so much as a policy of
//! committing after each statement. What varies, and what matters
//! operationally, is the *kind* of transaction and how long it is held.
//!
//! Two properties shape this module:
//!
//! * Firebird reclaims old record versions only up to the **oldest
//!   active transaction**. A transaction held open across a coffee break
//!   stalls garbage collection for every user of that database, so the
//!   status a session reports carries its age and the gap to the
//!   transaction counter, not just "open".
//! * **Lock resolution is not a detail.** `WAIT` makes a conflicting
//!   statement block until the other side finishes; `NO WAIT` fails it
//!   immediately. Which one a session uses decides whether a busy table
//!   hangs the UI or reports a conflict.
//!
//! Deliberately absent: savepoints, which are additive within an
//! existing transaction and can arrive later without reshaping any of
//! this, and `isc_tpb_consistency` (snapshot table stability), which
//! takes table-level locks and is too easy to fire at a production
//! server from a dropdown.

use serde::{Deserialize, Serialize};
use specta::Type;

/// When a session commits the statements the user runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum TxMode {
    /// Commit after every statement. The default, and what a user who
    /// has not thought about transactions expects.
    #[default]
    Autocommit,
    /// Hold statements in one transaction until the user commits or
    /// rolls back. Nothing is written until they say so — including
    /// DDL, which Firebird makes transactional.
    Manual,
}

/// Isolation level for an explicit transaction.
///
/// Firebird's third level, `consistency` (snapshot table stability), is
/// intentionally not offered: it takes table-level locks, so one user
/// selecting it from a dropdown can stall a shared server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum TxIsolation {
    /// Sees other transactions' commits as they happen. The cheapest
    /// option and the right default for interactive work.
    #[default]
    ReadCommitted,
    /// A stable view of the database from the moment the transaction
    /// started — Firebird's `concurrency`. Needed for a consistent
    /// multi-statement export; holds the snapshot, and therefore the
    /// oldest active transaction, for as long as it is open.
    Snapshot,
}

/// What a statement does when it hits a lock another transaction holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase", tag = "kind", content = "timeoutSecs")]
pub enum TxLocking {
    /// Block until the other transaction finishes. With `None` the wait
    /// is unbounded, which is how an editor ends up frozen on a row
    /// somebody else is editing.
    Wait(Option<u32>),
    /// Fail immediately on conflict. Predictable for interactive use,
    /// and the reason a conflicting statement reports rather than hangs.
    #[default]
    NoWait,
}

/// How a session's explicit transactions are started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TxConfig {
    /// Isolation level. Defaults to read-committed.
    pub isolation: TxIsolation,
    /// Lock resolution. Defaults to `NoWait` so the UI reports a
    /// conflict instead of blocking on it indefinitely.
    pub locking: TxLocking,
}

/// A session's current transaction state, as the UI shows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct TxStatus {
    /// Autocommit or manual.
    pub mode: TxMode,
    /// Settings explicit transactions are started with.
    pub config: TxConfig,
    /// Whether an explicit transaction is currently open. Always false
    /// in autocommit.
    pub open: bool,
    /// Statements run since the transaction opened, so the indicator can
    /// say what would be lost to a rollback.
    pub pending_statements: u32,
    /// How long the transaction has been open, in milliseconds. The
    /// number worth watching: a transaction open for minutes is holding
    /// back garbage collection for the whole database.
    pub age_ms: u64,
}

impl TxStatus {
    /// Status of a session that has no explicit transaction open.
    #[must_use]
    pub const fn closed(mode: TxMode, config: TxConfig) -> Self {
        Self {
            mode,
            config,
            open: false,
            pending_statements: 0,
            age_ms: 0,
        }
    }
}
