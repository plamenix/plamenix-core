//! Offline tests for the in-memory secret store.
//!
//! The keyring-backed [`KeyringStore`] cannot be exercised here without
//! a live macOS Keychain / Windows Credential Manager / libsecret
//! session; a follow-up integration test runs it from `just live-test`
//! once the test matrix exists.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_secrets::{InMemoryStore, SecretError, SecretRef, SecretStore};

fn fake_ref(account: &str) -> SecretRef {
    SecretRef::new("dev.plamenix.test", account)
}

#[test]
fn store_then_retrieve_round_trip() {
    let store = InMemoryStore::new();
    let key = fake_ref("profile:alpha:password");

    store.store(&key, "hunter2").unwrap();
    assert_eq!(store.retrieve(&key).unwrap(), "hunter2");
}

#[test]
fn retrieve_unknown_returns_not_found() {
    let store = InMemoryStore::new();
    let err = store
        .retrieve(&fake_ref("profile:ghost:password"))
        .unwrap_err();
    assert!(matches!(err, SecretError::NotFound { .. }));
}

#[test]
fn store_overwrites_previous_value() {
    let store = InMemoryStore::new();
    let key = fake_ref("profile:beta:password");

    store.store(&key, "old").unwrap();
    store.store(&key, "new").unwrap();
    assert_eq!(store.retrieve(&key).unwrap(), "new");
}

#[test]
fn delete_is_idempotent() {
    let store = InMemoryStore::new();
    let key = fake_ref("profile:gamma:password");

    store.delete(&key).unwrap();
    store.store(&key, "rotate").unwrap();
    store.delete(&key).unwrap();
    assert!(matches!(
        store.retrieve(&key),
        Err(SecretError::NotFound { .. }),
    ));
}
