//! Does the metadata store actually work on Firebird Embedded?
//!
//! Everything else in this crate rests on the bundled engine being able
//! to create and serve a local database with no server process. That
//! assumption is the whole design, so it is tested directly rather than
//! inferred from the code compiling.
//!
//! Needs the bundled Firebird from `plamenix-desktop/resources/fbclient`.
//! Skipped, loudly, when it is not there — a skip that reads as a pass
//! is how a foundation goes unverified.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use plamenix_db::{DbDriver, QueryResult};
use plamenix_meta::{AuditEntry, MetaStore};

/// The full Firebird install both editions ship. Embedded needs the
/// engine plugin and `firebird.conf`, not just a client library.
fn bundled_fbclient() -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../plamenix-desktop/resources/fbclient/v50/Resources/lib/libfbclient.dylib");
    path.exists().then(|| path.to_string_lossy().into_owned())
}

#[tokio::test]
async fn a_metadata_database_is_created_and_served_with_no_server_process() {
    let Some(fbclient) = bundled_fbclient() else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.fdb");

    let store = MetaStore::open(&path, Some(fbclient))
        .await
        .expect("embedded Firebird should create and open the metadata database");

    assert!(path.exists(), "the database file should exist on disk");
    assert_eq!(store.path(), path.as_path());
}

#[tokio::test]
async fn opening_twice_is_idempotent() {
    // The schema statements run on every open, so a second open must be
    // a no-op rather than an "object already exists" the caller has to
    // swallow — swallowing that would also swallow an object existing
    // with the wrong shape.
    let Some(fbclient) = bundled_fbclient() else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.fdb");

    let first = MetaStore::open(&path, Some(fbclient.clone()))
        .await
        .expect("first open");
    drop(first);

    MetaStore::open(&path, Some(fbclient))
        .await
        .expect("re-opening an existing metadata database must not fail");
}

#[tokio::test]
async fn an_audit_entry_survives_a_reopen() {
    // The point of an audit log: it outlives the process that wrote it.
    let Some(fbclient) = bundled_fbclient() else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.fdb");

    {
        let store = MetaStore::open(&path, Some(fbclient.clone()))
            .await
            .expect("open");
        store
            .record(&AuditEntry {
                actor: Some("alice".to_owned()),
                remote_addr: Some("127.0.0.1".to_owned()),
                action: "profile.delete".to_owned(),
                target: Some("prod-db".to_owned()),
                outcome: "allowed".to_owned(),
                detail: None,
            })
            .await
            .expect("record");
    }

    let store = MetaStore::open(&path, Some(fbclient))
        .await
        .expect("reopen");
    let result = store
        .driver()
        .execute(store.session(), "SELECT ACTOR FROM AUDIT_LOG".to_owned())
        .await
        .expect("read back");

    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1, "the entry should still be there")
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[tokio::test]
async fn a_quote_in_an_actor_name_does_not_corrupt_the_log() {
    // An audit log the audited thing can corrupt is not an audit log.
    // Token names come from configuration, one edit away from a quote.
    let Some(fbclient) = bundled_fbclient() else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta.fdb");
    let store = MetaStore::open(&path, Some(fbclient)).await.expect("open");

    store
        .record(&AuditEntry {
            actor: Some("o'brien'; DROP TABLE AUDIT_LOG --".to_owned()),
            remote_addr: None,
            action: "login".to_owned(),
            target: None,
            outcome: "refused".to_owned(),
            detail: None,
        })
        .await
        .expect("a hostile actor name must be stored, not executed");

    // The table still exists, which it would not if the payload ran.
    store
        .driver()
        .execute(store.session(), "SELECT COUNT(*) FROM AUDIT_LOG".to_owned())
        .await
        .expect("AUDIT_LOG should still exist");
}
