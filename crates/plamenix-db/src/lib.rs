//! Plamenix Firebird driver.
//!
//! `plamenix-db` owns the [`DbDriver`] trait — one of Plamenix's three
//! pragmatic swap-point traits — and its `rsfbclient`-backed default
//! implementation [`rsfb::RsfbDriver`]. Every other crate in Plamenix
//! (Tauri commands, the NAPI binding, plugin host) consumes Firebird
//! through this trait, never through `rsfbclient` directly.
//!
//! # Mental model
//!
//! * A driver is shared across the application (`Arc<dyn DbDriver>`).
//! * Each connect call returns a [`SessionId`]; that id keys every
//!   subsequent `execute` / `ping` / `close` call.
//! * Calls against the same `SessionId` serialise: rsfbclient and
//!   fbclient are not safe to call concurrently against one attachment.
//! * Synchronous rsfbclient work runs on Tokio's blocking pool.
//!
//! # Features
//!
//! * `native` (default) — links rsfbclient with native fbclient support
//!   (dynamic load of a user-supplied library path or the system linker).
//! * `pure-rust` — enables rsfbclient's pure-Rust wire-protocol backend.
//!
//! The two features are not mutually exclusive; both can be enabled and
//! [`driver::ConnectMode`] selects at connect time.
//!
//! See `../../../docs/firebird-driver.md` for the full driver design.

#![forbid(unsafe_code)]

pub mod crypt;
pub mod driver;
pub mod error;
pub mod query;
pub mod rsfb;

pub use crypt::CryptState;
pub use driver::{ConnectMode, DbDriver};
pub use error::DbError;
pub use query::{Column, ColumnValue, QueryResult, Row};
pub use rsfb::RsfbDriver;
pub use rsfb::resolver::{FBCLIENT_PATH_ENV, resolve_fbclient_path};

// Re-export for downstream crates that only depend on `plamenix-db`.
pub use plamenix_types::{ConnectionConfig, SessionId};
