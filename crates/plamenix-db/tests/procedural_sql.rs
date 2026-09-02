//! Live tests for procedural SQL: the statement splitter and the
//! row-producing / row-limit predicates, exercised through the real
//! driver against a real engine.
//!
//! These go through the whole chain — `split_statements` → the shells'
//! row-limit decision → `DbDriver::execute` — because every part of this
//! feature was individually "correct" while the product could not run a
//! stored procedure. Unit tests on the string helpers alone would have
//! stayed green through that.
//!
//! Requires the Firebird 5 container:
//!
//! ```sh
//! cd plamenix/dev/firebird5 && docker compose up -d
//! cargo test -p plamenix-db --test procedural_sql -- --ignored
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_db::{
    ColumnValue, ConnectMode, ConnectionConfig, DbDriver, QueryResult, RsfbDriver, SessionId,
    accepts_row_limit, inject_row_limit, split_statements,
};

const HOST: &str = "127.0.0.1";
const PORT: u16 = 3050;
const DB_PATH: &str = "/var/lib/firebird/data/test.fdb";
const ROW_LIMIT: u32 = 10_000;

fn config() -> ConnectionConfig {
    ConnectionConfig {
        host: HOST.into(),
        port: PORT,
        database: DB_PATH.into(),
        user: "SYSDBA".into(),
        password: "masterkey".into(),
        encryption_key: None,
        fbclient_path: None,
        encryption_required: false,
        charset: None,
        embedded: false,
    }
}

async fn connect(driver: &RsfbDriver) -> SessionId {
    driver
        .connect(config(), ConnectMode::PureRust)
        .await
        .expect("connect to the dev Firebird 5 container")
}

/// Runs a batch exactly as the shells do: split it, apply the row cap
/// only where the grammar allows one, execute in order.
async fn run_batch(driver: &RsfbDriver, session: SessionId, sql: &str) -> Vec<QueryResult> {
    let mut out = Vec::new();
    for stmt in split_statements(sql) {
        let exec_sql = if accepts_row_limit(&stmt) {
            inject_row_limit(&stmt, ROW_LIMIT)
        } else {
            stmt.clone()
        };
        let result = driver
            .execute(session, exec_sql)
            .await
            .unwrap_or_else(|err| panic!("statement failed: {stmt}\n  {err}"));
        out.push(result);
    }
    out
}

fn first_cell(result: &QueryResult) -> &ColumnValue {
    match result {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .unwrap_or_else(|| panic!("expected at least one row, got none"))
            .cells
            .first()
            .expect("expected at least one column"),
        QueryResult::Affected { rows } => {
            panic!("expected rows, got an affected-row count of {rows}")
        }
    }
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn execute_block_returns_its_rows() {
    // Before the fix this failed twice over: the batch was torn at the
    // semicolons inside the block, and a ROWS clause was appended to
    // what remained, which Firebird rejects with "Token unknown - ROWS".
    let driver = RsfbDriver::new();
    let session = connect(&driver).await;

    let results = run_batch(
        &driver,
        session,
        "EXECUTE BLOCK RETURNS (N INTEGER) AS BEGIN N = 42; SUSPEND; END;",
    )
    .await;

    assert_eq!(results.len(), 1, "batch was split");
    assert_eq!(
        first_cell(&results[0]),
        &ColumnValue::Integer(42),
        "block did not return its row",
    );

    driver.close(session).await.expect("close");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn set_term_script_creates_and_runs_a_procedure() {
    let driver = RsfbDriver::new();
    let session = connect(&driver).await;

    // The shape every Firebird script uses, straight out of isql or
    // IBExpert. The SET TERM directives are client-side and must never
    // reach the server.
    run_batch(
        &driver,
        session,
        "SET TERM ^ ;\n\
         CREATE OR ALTER PROCEDURE SP_SPLIT_TEST (A INTEGER, B INTEGER)\n\
         RETURNS (S INTEGER) AS BEGIN S = A + B; SUSPEND; END^\n\
         SET TERM ; ^",
    )
    .await;

    let results = run_batch(&driver, session, "EXECUTE PROCEDURE SP_SPLIT_TEST(2, 3);").await;
    assert_eq!(
        first_cell(&results[0]),
        &ColumnValue::Integer(5),
        "EXECUTE PROCEDURE output was discarded",
    );

    run_batch(&driver, session, "DROP PROCEDURE SP_SPLIT_TEST;").await;
    driver.close(session).await.expect("close");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn plain_selects_still_get_their_row_cap() {
    // The predicate split must not cost SELECT its truncation guard.
    let driver = RsfbDriver::new();
    let session = connect(&driver).await;

    let stmt = "SELECT 1 FROM rdb$database";
    assert!(accepts_row_limit(stmt));
    let results = run_batch(&driver, session, stmt).await;
    assert!(matches!(results[0], QueryResult::Rows { .. }));

    driver.close(session).await.expect("close");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn dml_still_reports_an_affected_count() {
    let driver = RsfbDriver::new();
    let session = connect(&driver).await;

    // RECREATE rather than CREATE: a failed earlier run must not leave
    // the table behind and break every run after it.
    run_batch(
        &driver,
        session,
        "RECREATE TABLE T_SPLIT_TEST (ID INTEGER NOT NULL PRIMARY KEY);",
    )
    .await;
    let results = run_batch(
        &driver,
        session,
        "INSERT INTO T_SPLIT_TEST (ID) VALUES (1);",
    )
    .await;
    assert!(
        matches!(results[0], QueryResult::Affected { .. }),
        "DML should report an affected count, got {:?}",
        results[0],
    );

    // Deliberately not dropped here. Firebird refuses the metadata lock
    // while the inserting transaction still holds the table
    // ("unsuccessful metadata update"), which is a property of the
    // driver's current commit timing rather than of this test. The
    // RECREATE above makes the next run clean regardless, and explicit
    // transaction control is the next item in this wave.
    driver.close(session).await.expect("close");
}
