//! History and grants, against the real engine.
//!
//! These replace two SQLite implementations of the same tables — one in
//! Rust, one in TypeScript — so the shapes they assert are the shapes
//! both editions previously relied on separately.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use plamenix_meta::{MetaStore, NewHistoryEntry};

fn bundled_fbclient() -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../plamenix-desktop/resources/fbclient/v50/Resources/lib/libfbclient.dylib");
    path.exists().then(|| path.to_string_lossy().into_owned())
}

async fn store(dir: &std::path::Path) -> Option<MetaStore> {
    let fbclient = bundled_fbclient()?;
    Some(
        MetaStore::open(dir.join("meta.fdb"), Some(fbclient))
            .await
            .expect("open"),
    )
}

fn entry<'a>(profile: &'a str, sql: &'a str, at: i64) -> NewHistoryEntry<'a> {
    NewHistoryEntry {
        profile_id: profile,
        sql,
        executed_at: at,
        duration_ms: 5,
        status: "ok",
        error: None,
        rows_returned: Some(1),
        label: None,
    }
}

#[tokio::test]
async fn history_round_trips_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let Some(store) = store(dir.path()).await else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };

    store
        .record_history(&entry("p1", "SELECT 1", 100))
        .await
        .unwrap();
    store
        .record_history(&entry("p1", "SELECT 2", 200))
        .await
        .unwrap();
    store
        .record_history(&entry("p2", "SELECT 3", 300))
        .await
        .unwrap();

    let listed = store.list_history("p1", 10).await.unwrap();
    assert_eq!(
        listed.len(),
        2,
        "history is scoped to the profile asked for"
    );
    assert_eq!(listed[0].sql, "SELECT 2", "newest first");
}

#[tokio::test]
async fn a_quote_in_sql_is_stored_not_executed() {
    // History records whatever the user typed, including a statement
    // that failed because it was malformed. Storing it must not run it.
    let dir = tempfile::tempdir().unwrap();
    let Some(store) = store(dir.path()).await else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };

    let hostile = "SELECT 'it''s'; DROP TABLE QUERY_HISTORY; --";
    store
        .record_history(&entry("p1", hostile, 1))
        .await
        .unwrap();

    let listed = store.list_history("p1", 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].sql, hostile);
}

#[tokio::test]
async fn history_can_be_labelled_deleted_and_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let Some(store) = store(dir.path()).await else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };

    for i in 0..3 {
        store
            .record_history(&entry("p1", "SELECT 1", i))
            .await
            .unwrap();
    }
    let listed = store.list_history("p1", 10).await.unwrap();
    let first = listed[0].id;

    assert!(store.set_history_label(first, Some("keep")).await.unwrap());
    assert_eq!(
        store.list_history("p1", 10).await.unwrap()[0]
            .label
            .as_deref(),
        Some("keep"),
    );

    assert!(store.delete_history(first).await.unwrap());
    assert_eq!(store.list_history("p1", 10).await.unwrap().len(), 2);

    assert_eq!(store.clear_history("p1").await.unwrap(), 2);
    assert!(store.list_history("p1", 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn deleting_no_ids_is_not_a_syntax_error() {
    // `IN ()` does not parse, and "delete nothing" is a reasonable
    // thing for a caller with an empty selection to ask.
    let dir = tempfile::tempdir().unwrap();
    let Some(store) = store(dir.path()).await else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };
    assert_eq!(store.delete_history_many(&[]).await.unwrap(), 0);
}

#[tokio::test]
async fn granting_twice_is_not_an_error() {
    // The pair is the primary key. A second grant of the same
    // capability is the user clicking twice, not a conflict.
    let dir = tempfile::tempdir().unwrap();
    let Some(store) = store(dir.path()).await else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };

    store.grant("dev.plamenix.x", "db.read.any").await.unwrap();
    store.grant("dev.plamenix.x", "db.read.any").await.unwrap();

    assert_eq!(
        store.granted_for("dev.plamenix.x").await.unwrap(),
        ["db.read.any"]
    );
}

#[tokio::test]
async fn revoking_what_was_never_granted_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let Some(store) = store(dir.path()).await else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };
    store
        .revoke("dev.plamenix.x", "never.granted")
        .await
        .unwrap();
    assert!(
        store
            .granted_for("dev.plamenix.x")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn grants_are_readable_per_plugin_and_all_at_once() {
    // The all-at-once form is what the boot path replays into the
    // plugin host, so a plugin's capabilities survive a restart.
    let dir = tempfile::tempdir().unwrap();
    let Some(store) = store(dir.path()).await else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };

    store.grant("a", "db.read.any").await.unwrap();
    store.grant("a", "settings.read").await.unwrap();
    store.grant("b", "clipboard.read").await.unwrap();

    let all = store.all_grants().await.unwrap();
    assert_eq!(all.get("a").map(Vec::len), Some(2));
    assert_eq!(all.get("b").map(Vec::len), Some(1));

    store.purge_plugin("a").await.unwrap();
    assert!(store.granted_for("a").await.unwrap().is_empty());
    assert_eq!(store.granted_for("b").await.unwrap(), ["clipboard.read"]);
}

#[tokio::test]
async fn history_is_trimmed_to_its_cap() {
    // Without a cap the table grows one row per statement, forever. The
    // desktop shell passed a limit to its SQLite store; dropping it in
    // the move would have been an unbounded table nobody noticed until
    // it was large.
    let dir = tempfile::tempdir().unwrap();
    let Some(store) = store(dir.path()).await else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };

    for i in 0..10 {
        store
            .record_history(&entry("p1", "SELECT 1", i))
            .await
            .unwrap();
    }
    store.trim_history("p1", 4).await.unwrap();

    let listed = store.list_history("p1", 100).await.unwrap();
    assert_eq!(listed.len(), 4, "only the newest should survive");
    assert_eq!(listed[0].executed_at, 9, "and the newest should be kept");
}

#[tokio::test]
async fn trimming_below_the_cap_removes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let Some(store) = store(dir.path()).await else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };
    store
        .record_history(&entry("p1", "SELECT 1", 1))
        .await
        .unwrap();
    assert_eq!(store.trim_history("p1", 50).await.unwrap(), 0);
    assert_eq!(store.list_history("p1", 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn history_breaks_same_millisecond_ties_by_id() {
    // Statements run faster than `EXECUTED_AT` resolves to, so a batch
    // of five recorded in one millisecond is ordinary rather than a
    // corner case. Without an `ID` tiebreak the engine may return them
    // in any order, which showed up as the retention cap appearing to
    // drop the newest entry: the cap kept the right rows and the list
    // reported a different set as newest.
    let dir = tempfile::tempdir().unwrap();
    let Some(store) = store(dir.path()).await else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };

    for n in 1..=5 {
        store
            .record_history(&entry("tie", &format!("SELECT {n}"), 1_000))
            .await
            .unwrap();
    }

    let listed = store.list_history("tie", 10).await.unwrap();
    let sqls: Vec<_> = listed.iter().map(|e| e.sql.as_str()).collect();
    assert_eq!(
        sqls,
        ["SELECT 5", "SELECT 4", "SELECT 3", "SELECT 2", "SELECT 1"],
        "insertion order must decide when the timestamps are equal"
    );

    // And `FIRST n` must cut the tie at the same place the trim does,
    // or the panel shows rows the cap is about to delete.
    let top = store.list_history("tie", 3).await.unwrap();
    let top_sqls: Vec<_> = top.iter().map(|e| e.sql.as_str()).collect();
    assert_eq!(top_sqls, ["SELECT 5", "SELECT 4", "SELECT 3"]);
}

#[tokio::test]
async fn trim_keeps_the_newest_within_one_millisecond() {
    // The cap and the list have to agree. They order by different
    // clauses in different statements, so agreement is a thing to
    // assert rather than assume.
    let dir = tempfile::tempdir().unwrap();
    let Some(store) = store(dir.path()).await else {
        println!("SKIPPED: bundled fbclient not found");
        return;
    };

    for n in 1..=5 {
        store
            .record_history(&entry("cap", &format!("SELECT {n}"), 2_000))
            .await
            .unwrap();
        store.trim_history("cap", 3).await.unwrap();
    }

    let listed = store.list_history("cap", 10).await.unwrap();
    let sqls: Vec<_> = listed.iter().map(|e| e.sql.as_str()).collect();
    assert_eq!(sqls, ["SELECT 5", "SELECT 4", "SELECT 3"]);
}
