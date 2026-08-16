//! The dev secret store's file permissions.
//!
//! `JsonSecretStore` is debug-builds-only and holds a developer's own
//! credentials in plain text, so 0600 is the whole protection. The type
//! documents that guarantee, which makes it worth a test rather than a
//! comment.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use plamenix_secrets::{JsonSecretStore, SecretRef, SecretStore};

#[test]
fn the_secrets_file_is_never_readable_by_anyone_else() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dev-secrets.json");
    let store = JsonSecretStore::open(path.clone()).unwrap();

    store
        .store(
            &SecretRef {
                service: "plamenix".into(),
                account: "sysdba@localhost".into(),
            },
            "masterkey-but-real",
        )
        .unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "secrets file is {mode:o}, readable by someone other than its owner",
    );
}

#[test]
fn a_rewrite_does_not_widen_the_permissions() {
    // The file is replaced through a temp file and a rename, so every
    // write is a fresh inode and has to get this right again.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dev-secrets.json");
    let store = JsonSecretStore::open(path.clone()).unwrap();
    let entry = SecretRef {
        service: "plamenix".into(),
        account: "a".into(),
    };

    store.store(&entry, "one").unwrap();
    store.store(&entry, "two").unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "rewrite widened the file to {mode:o}");
}
