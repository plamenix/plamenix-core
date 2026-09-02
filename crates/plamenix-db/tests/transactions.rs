//! Live tests for explicit transaction control.
//!
//! Exercised through the driver against a real engine, because the
//! behaviour that matters here — whether a rollback actually discards
//! work, whether a second session can see it — is invisible to a unit
//! test.
//!
//! ```sh
//! cd plamenix/dev/firebird5 && docker compose up -d
//! cargo test -p plamenix-db --test transactions -- --ignored
//! ```
//!
//! **One live suite at a time.** These do DDL, Firebird serialises DDL
//! per database, and cargo runs test binaries in parallel — so
//! `cargo test -p plamenix-db -- --ignored` puts several suites in
//! contention for the same metadata lock and one of them loses. Named
//! here because the symptom is misleading: the loser fails on a row
//! count several lines after the DDL that actually failed.
//!
//! Both engines pass. For 2.5: `PLAMENIX_TEST_PORT=3051
//! PLAMENIX_TEST_DB=/firebird/data/test.fdb`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_db::{
    ColumnValue, ConnectMode, ConnectionConfig, DbDriver, QueryResult, RsfbDriver, SessionId,
    TxConfig, TxIsolation, TxLocking, TxMode,
};

fn config() -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        // Overridable so the same suite can run against the Firebird 2.5
        // container (port 3051, a different data path) as well as 5.0.
        // See plamenix/dev/README.md.
        port: std::env::var("PLAMENIX_TEST_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3050),
        database: std::env::var("PLAMENIX_TEST_DB")
            .unwrap_or_else(|_| "/var/lib/firebird/data/test.fdb".into()),
        user: "SYSDBA".into(),
        password: "masterkey".into(),
        encryption_key: None,
        fbclient_path: None,
        encryption_required: false,
        charset: None,
        embedded: false,
    }
}

fn manual() -> TxConfig {
    TxConfig {
        isolation: TxIsolation::ReadCommitted,
        locking: TxLocking::NoWait,
    }
}

async fn open(driver: &RsfbDriver) -> SessionId {
    driver
        .connect(config(), ConnectMode::PureRust)
        .await
        .expect("connect")
}

async fn run(driver: &RsfbDriver, session: SessionId, sql: &str) -> QueryResult {
    driver
        .execute(session, sql.to_string())
        .await
        .unwrap_or_else(|err| panic!("{sql}\n  {err}"))
}

async fn count(driver: &RsfbDriver, session: SessionId, table: &str) -> i64 {
    let result = run(driver, session, &format!("SELECT COUNT(*) FROM {table}")).await;
    match result {
        QueryResult::Rows { rows, .. } => match rows[0].cells[0] {
            ColumnValue::Integer(v) => v,
            ref other => panic!("expected integer, got {other:?}"),
        },
        QueryResult::Affected { .. } => panic!("expected rows"),
    }
}

/// Creates a scratch table, committed, so each test starts from a known
/// state without depending on the shared seed schema.
/// Gives a test an empty table of its own.
///
/// The result is checked rather than discarded, and that matters more
/// than it looks. `RECREATE TABLE` is DDL, and Firebird serialises DDL
/// per database — so when cargo runs these test binaries in parallel
/// against the one container, a recreate can lose the metadata lock to
/// a concurrent suite. Swallowing that left the *previous* run's table
/// in place, rows and all, and the test then failed several lines later
/// on a row count that was off by whatever the last run inserted. The
/// symptom pointed at transaction semantics; the cause was a discarded
/// `Result`.
///
/// Retried because the contention is transient, and asserted because a
/// recreate that genuinely cannot happen should say so here rather than
/// as a mystery downstream.
async fn scratch_table(driver: &RsfbDriver, session: SessionId, name: &str) {
    let sql = format!("RECREATE TABLE {name} (ID INTEGER)");
    let mut last = String::new();
    for attempt in 0..10 {
        match driver.execute(session, sql.clone()).await {
            Ok(_) => return,
            Err(err) => {
                last = err.to_string();
                // Back off a little; the holder is another test binary
                // finishing its own DDL, not a human.
                tokio::time::sleep(std::time::Duration::from_millis(50 * (attempt + 1))).await;
            }
        }
    }
    panic!("could not create scratch table {name}: {last}");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn rollback_discards_the_work() {
    let driver = RsfbDriver::new();
    let session = open(&driver).await;
    scratch_table(&driver, session, "T_TX_ROLLBACK").await;

    driver
        .set_transaction_mode(session, TxMode::Manual, manual())
        .await
        .expect("switch to manual");

    run(
        &driver,
        session,
        "INSERT INTO T_TX_ROLLBACK (ID) VALUES (1)",
    )
    .await;
    run(
        &driver,
        session,
        "INSERT INTO T_TX_ROLLBACK (ID) VALUES (2)",
    )
    .await;

    let status = driver.transaction_status(session).await.expect("status");
    assert!(status.open, "manual mode should have opened a transaction");
    assert_eq!(status.pending_statements, 2);

    // Read-your-own-writes: the user's SELECT runs on the same
    // attachment, so it must see the uncommitted rows.
    assert_eq!(count(&driver, session, "T_TX_ROLLBACK").await, 2);

    let status = driver.rollback(session).await.expect("rollback");
    assert!(!status.open);
    assert_eq!(status.pending_statements, 0);
    assert_eq!(count(&driver, session, "T_TX_ROLLBACK").await, 0);

    driver.close(session).await.expect("close");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn commit_makes_the_work_visible_to_another_session() {
    let driver = RsfbDriver::new();
    let writer = open(&driver).await;
    let reader = open(&driver).await;
    scratch_table(&driver, writer, "T_TX_COMMIT").await;

    driver
        .set_transaction_mode(writer, TxMode::Manual, manual())
        .await
        .expect("switch to manual");
    run(&driver, writer, "INSERT INTO T_TX_COMMIT (ID) VALUES (1)").await;

    // Uncommitted work must stay invisible to everyone else.
    assert_eq!(count(&driver, reader, "T_TX_COMMIT").await, 0);

    driver.commit(writer).await.expect("commit");
    assert_eq!(count(&driver, reader, "T_TX_COMMIT").await, 1);

    driver.close(writer).await.expect("close writer");
    driver.close(reader).await.expect("close reader");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn ddl_rolls_back_too() {
    // Firebird makes DDL transactional, which is worth having and worth
    // proving — a dropped table can be taken back.
    let driver = RsfbDriver::new();
    let session = open(&driver).await;
    scratch_table(&driver, session, "T_TX_DDL").await;

    driver
        .set_transaction_mode(session, TxMode::Manual, manual())
        .await
        .expect("switch to manual");
    run(&driver, session, "DROP TABLE T_TX_DDL").await;
    driver.rollback(session).await.expect("rollback");

    // Still there: the DROP was undone.
    assert_eq!(count(&driver, session, "T_TX_DDL").await, 0);

    driver.close(session).await.expect("close");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn typed_commit_and_rollback_route_to_the_api() {
    let driver = RsfbDriver::new();
    let session = open(&driver).await;
    scratch_table(&driver, session, "T_TX_TYPED").await;

    driver
        .set_transaction_mode(session, TxMode::Manual, manual())
        .await
        .expect("switch to manual");
    run(&driver, session, "INSERT INTO T_TX_TYPED (ID) VALUES (1)").await;

    // Typed into the editor rather than pressed on the toolbar. It used
    // to reach the engine and fail, because rsfbclient owns the
    // transaction handle.
    run(&driver, session, "COMMIT;").await;
    assert!(!driver.transaction_status(session).await.unwrap().open);
    assert_eq!(count(&driver, session, "T_TX_TYPED").await, 1);

    run(&driver, session, "INSERT INTO T_TX_TYPED (ID) VALUES (2)").await;
    run(&driver, session, "rollback work").await;
    assert_eq!(count(&driver, session, "T_TX_TYPED").await, 1);

    driver.close(session).await.expect("close");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn autocommit_is_the_default_and_holds_nothing_open() {
    let driver = RsfbDriver::new();
    let session = open(&driver).await;
    scratch_table(&driver, session, "T_TX_AUTO").await;

    let status = driver.transaction_status(session).await.expect("status");
    assert_eq!(status.mode, TxMode::Autocommit);
    assert!(!status.open);

    run(&driver, session, "INSERT INTO T_TX_AUTO (ID) VALUES (1)").await;
    assert!(
        !driver.transaction_status(session).await.unwrap().open,
        "autocommit must not leave a transaction open",
    );

    driver.close(session).await.expect("close");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn closing_with_work_outstanding_rolls_back_rather_than_commits() {
    // Detaching is not consent to write.
    let driver = RsfbDriver::new();
    let writer = open(&driver).await;
    let reader = open(&driver).await;
    scratch_table(&driver, writer, "T_TX_CLOSE").await;

    driver
        .set_transaction_mode(writer, TxMode::Manual, manual())
        .await
        .expect("switch to manual");
    run(&driver, writer, "INSERT INTO T_TX_CLOSE (ID) VALUES (1)").await;
    driver.close(writer).await.expect("close");

    assert_eq!(count(&driver, reader, "T_TX_CLOSE").await, 0);
    driver.close(reader).await.expect("close reader");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn mode_cannot_change_underneath_an_open_transaction() {
    let driver = RsfbDriver::new();
    let session = open(&driver).await;
    scratch_table(&driver, session, "T_TX_MODE").await;

    driver
        .set_transaction_mode(session, TxMode::Manual, manual())
        .await
        .expect("switch to manual");
    run(&driver, session, "INSERT INTO T_TX_MODE (ID) VALUES (1)").await;

    // Flipping back to autocommit here would have to either commit or
    // discard the open work; refusing is the only answer that does not
    // silently pick one.
    let err = driver
        .set_transaction_mode(session, TxMode::Autocommit, manual())
        .await
        .expect_err("mode change should be refused");
    assert!(
        err.to_string().contains("commit or roll back"),
        "unhelpful error: {err}",
    );

    driver.rollback(session).await.expect("rollback");
    driver
        .set_transaction_mode(session, TxMode::Autocommit, manual())
        .await
        .expect("mode change after rollback");

    driver.close(session).await.expect("close");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn a_failed_statement_leaves_the_transaction_usable() {
    let driver = RsfbDriver::new();
    let session = open(&driver).await;
    scratch_table(&driver, session, "T_TX_FAIL").await;

    driver
        .set_transaction_mode(session, TxMode::Manual, manual())
        .await
        .expect("switch to manual");
    run(&driver, session, "INSERT INTO T_TX_FAIL (ID) VALUES (1)").await;

    driver
        .execute(session, "SELECT * FROM NO_SUCH_TABLE".into())
        .await
        .expect_err("statement should fail");

    let status = driver.transaction_status(session).await.expect("status");
    assert!(
        status.open,
        "a failed statement must not close the transaction"
    );
    assert_eq!(
        status.pending_statements, 1,
        "a failed statement contributes nothing a rollback would discard",
    );

    // The user can correct course and carry on.
    run(&driver, session, "INSERT INTO T_TX_FAIL (ID) VALUES (2)").await;
    assert_eq!(count(&driver, session, "T_TX_FAIL").await, 2);
    driver.rollback(session).await.expect("rollback");
    assert_eq!(count(&driver, session, "T_TX_FAIL").await, 0);

    driver.close(session).await.expect("close");
}
