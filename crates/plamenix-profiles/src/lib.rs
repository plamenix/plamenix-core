//! Persistent connection profiles for Plamenix.
//!
//! A [`Profile`] is the serialisable record a user saves so they can
//! reconnect to a Firebird database without re-entering host details.
//! Profiles never persist plaintext secrets: the password and any
//! encryption key live in `plamenix-secrets` (OS keyring) and the
//! profile carries the keyring account key as a [`SecretRef`]-style
//! string. [`resolve_connection_config`] consumes a profile plus a
//! [`plamenix_secrets::SecretStore`] and produces a
//! [`plamenix_types::ConnectionConfig`] ready to hand to
//! `DbDriver::connect`.
//!
//! [`JsonFileStore`] is the on-disk implementation used by the desktop
//! edition (storing `profiles.json` under the application's
//! config dir) and by the web edition's server-side store. The store
//! interface is dyn-compatible so future backends (cloud sync,
//! encrypted bundle) can slot in without touching call sites.

#![forbid(unsafe_code)]

pub mod error;
pub mod profile;
pub mod resolve;
pub mod store;

pub use error::ProfileError;
pub use profile::{Profile, ProfileId};
pub use resolve::{ConnectOverrides, RuntimeSecrets, resolve_connection_config, resolve_pure_rust};
pub use store::{JsonFileStore, ProfileStore};
