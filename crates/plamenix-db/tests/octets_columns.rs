//! Live tests for OCTETS columns.
//!
//! A `BINARY(n)` column (Firebird 4+ shorthand) — and any
//! `CHAR(n)/VARCHAR(n) CHARACTER SET OCTETS` before it — carries raw
//! bytes with charset subtype 1 on the wire. Both backends used to
//! strict-decode those bytes in the connection charset, so a table
//! with a UUID column crashed every `SELECT` that touched it:
//!
//! ```text
//! query failed: error: Found column with an invalid UTF-8 string:
//! invalid utf-8 sequence of 1 bytes from index 3
//! ```
//!
//! (plamenix-desktop#3.) The fix hands OCTETS columns up as
//! `SqlType::Binary`, which the driver surfaces as a blob value with
//! a hex peek. These tests pin that behaviour against a live server.
//!
//! Point them at any engine that can create the schema:
//!
//! ```sh
//! cargo test -p plamenix-db --test octets_columns -- --ignored
//! PLAMENIX_TEST_PORT=3050 PLAMENIX_TEST_DB=/firebird/data/uuidtest.fdb \
//!   PLAMENIX_FBCLIENT_PATH=/path/to/fbclient.dll \
//!   cargo test -p plamenix-db --test octets_columns -- --ignored
//! ```
//!
//! The schema is created by the test itself, mirroring the original
//! report: `CREATE DOMAIN D_UUID BINARY(16)` where the engine is new
//! enough to accept it, `CHAR(16) CHARACTER SET OCTETS` (identical on
//! the wire) otherwise. The database file is left behind on failure —
//! drop it manually when cleaning up.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_db::{
    ColumnValue, ConnectMode, ConnectionConfig, DbDriver, QueryResult, RsfbDriver,
};

fn config() -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: std::env::var("PLAMENIX_TEST_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3050),
        database: std::env::var("PLAMENIX_TEST_DB")
            .unwrap_or_else(|_| "/var/lib/firebird/data/uuidtest.fdb".into()),
        user: "SYSDBA".into(),
        password: "masterkey".into(),
        encryption_key: None,
        fbclient_path: std::env::var("PLAMENIX_FBCLIENT_PATH").ok(),
        encryption_required: false,
        charset: None,
        embedded: false,
    }
}

/// Creates the domain the report used, on any engine version. `true`
/// means the engine accepted the `BINARY(16)` shorthand (FB 4+);
/// `false` means we fell back to the identical-on-the-wire explicit
/// `CHARACTER SET OCTETS` form that Firebird 2.5 / 3.0 understand.
async fn create_domain(driver: &RsfbDriver, session: plamenix_types::SessionId) -> bool {
    if driver
        .execute(session, "CREATE DOMAIN D_UUID BINARY(16);".into())
        .await
        .is_ok()
    {
        return true;
    }
    driver
        .execute(
            session,
            "CREATE DOMAIN D_UUID AS CHAR(16) CHARACTER SET OCTETS;".into(),
        )
        .await
        .expect("OCTETS domain should be creatable on every engine");
    false
}

async fn open_with_schema(mode: ConnectMode) -> (RsfbDriver, plamenix_types::SessionId) {
    let driver = RsfbDriver::new();
    let session = driver
        .connect(config(), mode)
        .await
        .expect("connect to the live test server");
    // The schema outlives the test (the database is shared across runs
    // and both tests), so start from a clean slate. Ignore the errors:
    // on a fresh database both drops have nothing to drop.
    let _ = driver
        .execute(session, "DROP TABLE \"tblUuidTest\";".into())
        .await;
    let _ = driver
        .execute(session, "DROP DOMAIN D_UUID;".into())
        .await;
    let _ = create_domain(&driver, session).await;
    driver
        .execute(
            session,
            "CREATE TABLE \"tblUuidTest\" (\"userUuid\" D_UUID NOT NULL);".into(),
        )
        .await
        .expect("table create should succeed");
    driver
        .execute(
            session,
            "INSERT INTO \"tblUuidTest\" VALUES (CHAR_TO_UUID('3d6d2caf-64dc-4c42-8fd2-895a4ad04b60'));"
                .into(),
        )
        .await
        .expect("insert should succeed");
    (driver, session)
}

/// Both backends against the same live schema, sequentially — parallel
/// tests would race each other's DROP/CREATE on the shared database.
#[tokio::test]
#[ignore = "needs running Firebird"]
async fn selecting_an_octets_uuid_column_returns_bytes_not_an_error() {
    let expected_hex = "3d6d2caf64dc4c428fd2895a4ad04b60";

    // Native backend (bundled fbclient) …
    let (driver, session) = open_with_schema(ConnectMode::Native).await;
    let result = driver
        .execute(session, "SELECT \"userUuid\" FROM \"tblUuidTest\";".into())
        .await
        .expect("a BINARY(16) column must select without a UTF-8 error");
    assert_blob_with_peek(&result, 16, expected_hex);
    driver.close(session).await.expect("close");

    // … and the pure-Rust wire backend against the same database.
    let (driver, session) = open_with_schema(ConnectMode::PureRust).await;
    let result = driver
        .execute(session, "SELECT \"userUuid\" FROM \"tblUuidTest\";".into())
        .await
        .expect("a BINARY(16) column must select without a UTF-8 error");
    assert_blob_with_peek(&result, 16, expected_hex);
    driver.close(session).await.expect("close");
}

fn assert_blob_with_peek(result: &QueryResult, size: i64, hex: &str) {
    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Affected { .. } => panic!("expected rows"),
    };
    assert_eq!(rows.len(), 1);
    // Exact bytes of 3d6d2caf-64dc-4c42-8fd2-895a4ad04b60,
    // CHAR_TO_UUID's mixed-endian encoding.
    match &rows[0].cells[0] {
        ColumnValue::Blob(blob) => {
            assert_eq!(blob.size_bytes, size, "the raw column is 16 bytes");
            assert_eq!(
                blob.peek_hex.to_lowercase(),
                hex,
                "the peek should carry the UUID's raw bytes"
            );
        }
        other => panic!("expected a blob value for the OCTETS column, got {other:?}"),
    }
}
