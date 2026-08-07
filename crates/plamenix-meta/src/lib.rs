//! Plamenix's own metadata, in Firebird.
//!
//! Query history, plugin grants, and the audit log used to live in
//! SQLite — twice. The desktop edition kept them in `rusqlite` and the
//! web edition kept the identical schema in `better-sqlite3`, so the
//! same table existed in two languages with two migration paths and two
//! chances to drift.
//!
//! They now live in one Firebird database, opened in embedded mode, and
//! one implementation serves both editions.
//!
//! ## Why Firebird and not SQLite
//!
//! The obvious objection to SQLite in a Firebird IDE is that it is odd,
//! and the obvious defence is that app-local metadata is what SQLite is
//! for. The defence turns out to be weaker than it looks:
//!
//! * **The engine is already shipped.** `resources/fbclient/` carries
//!   `libEngine13`, `firebird.conf`, and `firebird.msg` — everything
//!   embedded mode needs. It costs no extra bytes, which was the main
//!   argument against.
//! * **It removes a duplicate implementation** rather than adding one.
//! * **It runs the driver on every app start.** A bug in our own
//!   Firebird driver now shows up when the IDE opens, not when a user
//!   finds it.
//!
//! ## What embedded mode costs
//!
//! Exclusive access. Firebird's embedded engine holds the file, so two
//! processes cannot open the same metadata database — see
//! [`MetaError::Locked`], which exists to say that in words rather than
//! let a second instance fail obscurely. Both editions are
//! single-process by design, but a stale process after a crash is a
//! real situation and it should read as one.
//!
//! Embedded also requires the native driver. The pure-Rust driver
//! implements the wire protocol, and embedded has no wire.

#![forbid(unsafe_code)]

mod grants;
mod history;
mod schema;
mod store;

pub use history::NewHistoryEntry;
pub use schema::SCHEMA_STATEMENTS;
pub use store::{AuditEntry, MetaError, MetaStore};
