//! Declared column types, against a real engine.
//!
//! **One live suite at a time** — see `transactions.rs`.
//!
//! These render into DDL exports and drive the inline editor's
//! validation, so getting them wrong is not cosmetic: a `NUMERIC(18,4)`
//! rendered bare is read back by Firebird as `NUMERIC(9,0)`, which
//! silently changes the column's type and truncates every value in it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_db::{ConnectMode, ConnectionConfig, DbDriver, RsfbDriver, SessionId};

fn config() -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        // Overridable so the same suite runs against the Firebird 2.5
        // container as well as 5.0. See plamenix/dev/README.md.
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

async fn open(driver: &RsfbDriver) -> SessionId {
    driver
        .connect(config(), ConnectMode::PureRust)
        .await
        .expect("connect")
}

/// Runs DDL and says so when it does not.
///
/// Not `let _ =`. A swallowed `RECREATE` leaves the previous run's table
/// in place — or no table at all — and the assertion then fails several
/// lines later on something unrelated. That exact mistake is why the
/// transaction suite failed for a day.
async fn run(driver: &RsfbDriver, session: SessionId, sql: &str) {
    // Retried, then asserted. Firebird serialises DDL per database and
    // both tests here create a table, so one can lose the metadata lock
    // to the other; that is transient. What is not acceptable is
    // swallowing it — a `RECREATE` that quietly failed leaves the
    // previous run's table in place and the assertion then fails
    // several lines later on something unrelated.
    let mut last = String::new();
    for attempt in 0..10 {
        match driver.execute(session, sql.to_owned()).await {
            Ok(_) => return,
            Err(err) => {
                last = err.to_string();
                tokio::time::sleep(std::time::Duration::from_millis(50 * (attempt + 1))).await;
            }
        }
    }
    panic!("failed to run `{sql}`: {last}");
}

/// The rendered type of one column of one table.
async fn column_type(driver: &RsfbDriver, session: SessionId, table: &str, column: &str) -> String {
    let schema = driver.describe_schema(session).await.expect("describe");
    schema
        .tables
        .iter()
        .find(|t| t.name == table)
        .unwrap_or_else(|| panic!("table {table} not in schema"))
        .columns
        .iter()
        .find(|c| c.name == column)
        .unwrap_or_else(|| panic!("column {column} not in {table}"))
        .sql_type
        .clone()
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn exact_numerics_keep_their_precision_and_scale() {
    let driver = RsfbDriver::new();
    let session = open(&driver).await;
    run(
        &driver,
        session,
        "RECREATE TABLE T_TYPES_NUM (
             MONEY NUMERIC(18,4),
             SMALL_DEC DECIMAL(4,2),
             MID NUMERIC(9,3),
             PLAIN_INT INTEGER,
             BIG BIGINT
         )",
    )
    .await;

    // The one that matters: Firebird reads a bare `NUMERIC` as
    // `NUMERIC(9,0)`, so a DDL export that dropped these truncated
    // every money value on the round trip.
    assert_eq!(
        column_type(&driver, session, "T_TYPES_NUM", "MONEY").await,
        "NUMERIC(18,4)",
    );
    assert_eq!(
        column_type(&driver, session, "T_TYPES_NUM", "SMALL_DEC").await,
        "DECIMAL(4,2)",
    );
    assert_eq!(
        column_type(&driver, session, "T_TYPES_NUM", "MID").await,
        "NUMERIC(9,3)",
    );

    // Plain integers must not grow a precision they never declared.
    assert_eq!(
        column_type(&driver, session, "T_TYPES_NUM", "PLAIN_INT").await,
        "INTEGER",
    );
    assert_eq!(
        column_type(&driver, session, "T_TYPES_NUM", "BIG").await,
        "BIGINT",
    );

    driver.close(session).await.expect("close");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn text_columns_report_characters_not_bytes() {
    // `RDB$FIELD_LENGTH` is bytes. In a UTF8 database that is four
    // times the declared width, so this used to render `VARCHAR(400)`
    // for a `VARCHAR(100)` — wrong DDL, and an inline editor that let
    // the user type well past what the column holds.
    let driver = RsfbDriver::new();
    let session = open(&driver).await;
    run(
        &driver,
        session,
        "RECREATE TABLE T_TYPES_TXT (
             NAME VARCHAR(100),
             CODE CHAR(8)
         )",
    )
    .await;

    assert_eq!(
        column_type(&driver, session, "T_TYPES_TXT", "NAME").await,
        "VARCHAR(100)",
    );
    assert_eq!(
        column_type(&driver, session, "T_TYPES_TXT", "CODE").await,
        "CHAR(8)",
    );

    driver.close(session).await.expect("close");
}
