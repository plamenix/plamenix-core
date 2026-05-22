//! Offline tests for the JSON-file profile store and the
//! secret-resolving connect-config helper.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_profiles::{
    ConnectOverrides, JsonFileStore, Profile, ProfileError, ProfileId, ProfileStore,
    RuntimeSecrets, resolve_connection_config, resolve_pure_rust,
};
use plamenix_secrets::{InMemoryStore, SecretRef, SecretStore};

const SERVICE: &str = "dev.plamenix.test";

fn sample_profile() -> Profile {
    Profile::new("Local Dev", "127.0.0.1", 3050, "/var/lib/firebird/data/test.fdb", "SYSDBA")
}

#[test]
fn list_on_missing_file_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().join("profiles.json"));
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn save_then_list_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().join("profiles.json"));
    let profile = sample_profile();
    let saved_id = profile.id;
    store.save(profile).unwrap();

    let profiles = store.list().unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].id, saved_id);
    assert_eq!(profiles[0].name, "Local Dev");
}

#[test]
fn save_updates_existing_profile() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().join("profiles.json"));

    let mut profile = sample_profile();
    let id = profile.id;
    store.save(profile.clone()).unwrap();

    profile.name = "Renamed".into();
    store.save(profile).unwrap();

    let fetched = store.get(id).unwrap();
    assert_eq!(fetched.name, "Renamed");
}

#[test]
fn delete_idempotent_and_get_404s() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().join("profiles.json"));
    let profile = sample_profile();
    let id = profile.id;
    store.save(profile).unwrap();

    store.delete(id).unwrap();
    store.delete(id).unwrap();
    assert!(matches!(store.get(id), Err(ProfileError::NotFound(_))));
}

#[test]
fn invalid_file_surfaces_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("profiles.json");
    std::fs::write(&path, b"{not json").unwrap();
    let store = JsonFileStore::new(&path);
    assert!(matches!(store.list(), Err(ProfileError::InvalidFile { .. })));
}

#[test]
fn resolve_uses_keyring_secret_when_ref_present() {
    let secrets = InMemoryStore::new();
    secrets
        .store(&SecretRef::new(SERVICE, "profile:demo:password"), "fromKeyring")
        .unwrap();

    let mut profile = sample_profile();
    profile.password_keyring_ref = Some("profile:demo:password".into());

    let config = resolve_connection_config(
        &profile,
        &secrets,
        SERVICE,
        &RuntimeSecrets::default(),
        &ConnectOverrides::default(),
    )
    .unwrap();
    assert_eq!(config.password, "fromKeyring");
    assert!(config.encryption_key.is_none());
}

#[test]
fn resolve_falls_back_to_runtime_password() {
    let secrets = InMemoryStore::new();
    let profile = sample_profile();
    let runtime = RuntimeSecrets {
        password: Some("masterkey".into()),
        encryption_key: None,
    };

    let config = resolve_connection_config(
        &profile,
        &secrets,
        SERVICE,
        &runtime,
        &ConnectOverrides::default(),
    )
    .unwrap();
    assert_eq!(config.password, "masterkey");
}

#[test]
fn resolve_propagates_overrides() {
    let secrets = InMemoryStore::new();
    let mut profile = sample_profile();
    profile.encryption_required = false;
    profile.pure_rust = false;

    let overrides = ConnectOverrides {
        pure_rust: Some(true),
        encryption_required: Some(true),
        fbclient_path: Some("/opt/firebird/lib/libfbclient.dylib".into()),
        charset: None,
    };

    let config = resolve_connection_config(
        &profile,
        &secrets,
        SERVICE,
        &RuntimeSecrets {
            password: Some("hunter2".into()),
            encryption_key: None,
        },
        &overrides,
    )
    .unwrap();

    assert!(config.encryption_required);
    assert_eq!(
        config.fbclient_path.as_deref(),
        Some("/opt/firebird/lib/libfbclient.dylib"),
    );
    assert!(resolve_pure_rust(&profile, &overrides));
}

#[test]
fn missing_keyring_secret_returns_error() {
    let secrets = InMemoryStore::new();
    let mut profile = sample_profile();
    profile.password_keyring_ref = Some("profile:ghost:password".into());

    let err = resolve_connection_config(
        &profile,
        &secrets,
        SERVICE,
        &RuntimeSecrets::default(),
        &ConnectOverrides::default(),
    )
    .unwrap_err();
    assert!(matches!(err, ProfileError::Secret(_)));
}

#[test]
fn profile_id_round_trips_through_json() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonFileStore::new(dir.path().join("profiles.json"));
    let id = ProfileId::new();
    let mut profile = sample_profile();
    profile.id = id;

    store.save(profile).unwrap();
    let fetched = store.get(id).unwrap();
    assert_eq!(fetched.id, id);
}
