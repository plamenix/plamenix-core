//! Plamenix secret store.
//!
//! `plamenix-secrets` wraps the OS keyring (macOS Keychain, Windows
//! Credential Manager, Linux Secret Service) behind the dyn-compatible
//! [`SecretStore`] trait. Plamenix uses it for two distinct secret
//! classes:
//!
//! * **Connection passwords** — written under
//!   `service = "dev.plamenix.<edition>"`, `account = "profile:<id>:password"`.
//! * **Encryption keys** — under
//!   `account = "profile:<id>:encryption-key"`.
//!
//! Both are opt-in per connection profile; the connect dialog stores
//! values only when the user explicitly ticks "remember in keyring".
//! Without an opt-in, secrets live in memory for the session and are
//! cleared on disconnect.
//!
//! Tests and headless CI builds use [`InMemoryStore`] instead of
//! [`KeyringStore`] to avoid touching the host's keychain.

#![forbid(unsafe_code)]

pub mod error;
pub mod store;

pub use error::SecretError;
pub use store::{InMemoryStore, KeyringStore, SecretRef, SecretStore};
