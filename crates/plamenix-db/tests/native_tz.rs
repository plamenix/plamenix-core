//! End-to-end test for native-mode connections against Firebird 5.
//!
//! Verifies that a connection built with `ConnectMode::Native`
//! plus a bundled `libfbclient` can attach to a live FB 5 server
//! and execute a regular query. Pure-Rust mode passes the same
//! query; the native path is exercised here to prove the
//! `with_dyn_load` plumbing and the resolver agree.
//!
//! The original ambition for this test was to round-trip
//! `CURRENT_TIMESTAMP` through `TIMESTAMP WITH TIME ZONE`, but
//! rsfbclient 0.26 does not yet convert FB 4+ TZ types in either
//! backend — see `plamenix/docs/firebird-driver.md`. Once an
//! upstream PR lands, switch the assertion below to a TZ column.
//!
//! Requires:
//!
//! * A reachable Firebird 5 server at `127.0.0.1:3050` with
//!   `/var/lib/firebird/data/test.fdb` and `SYSDBA:masterkey`.
//! * `PLAMENIX_FBCLIENT_PATH` pointing at a bundled `libfbclient`.
//! * macOS hosts also need `DYLD_LIBRARY_PATH` set to the framework's
//!   `Resources/lib/` so the `@rpath/lib/libtommath.dylib` reference
//!   inside `libfbclient.dylib` resolves.
//! * The `native` Cargo feature enabled:
//!   `cargo test -p plamenix-db --features native --test native_tz`.

#![cfg(feature = "native")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_db::{ColumnValue, ConnectMode, ConnectionConfig, DbDriver, QueryResult, RsfbDriver};

const FBCLIENT_PATH_ENV: &str = "PLAMENIX_FBCLIENT_PATH";
const HOST: &str = "127.0.0.1";
const PORT: u16 = 3050;
const DB_PATH: &str = "/var/lib/firebird/data/test.fdb";

fn live_config() -> Option<ConnectionConfig> {
    std::env::var(FBCLIENT_PATH_ENV)
        .ok()
        .map(|fbclient_path| ConnectionConfig {
            host: HOST.into(),
            port: PORT,
            database: DB_PATH.into(),
            user: "SYSDBA".into(),
            password: "masterkey".into(),
            encryption_key: None,
            fbclient_path: Some(fbclient_path),
            encryption_required: false,
            charset: None,
            embedded: false,
        })
}

#[tokio::test]
#[ignore = "needs running Firebird + PLAMENIX_FBCLIENT_PATH"]
async fn native_mode_connects_and_runs_query() {
    let Some(config) = live_config() else {
        panic!("PLAMENIX_FBCLIENT_PATH not set; see file header for setup");
    };

    let driver = RsfbDriver::new();
    let session = driver
        .connect(config, ConnectMode::Native)
        .await
        .expect("native connect should succeed");

    let result = driver
        .execute(
            session,
            "SELECT CAST(42 AS INTEGER) AS answer, CAST('plamenix' AS VARCHAR(32)) AS name \
             FROM RDB$DATABASE"
                .into(),
        )
        .await
        .expect("query should succeed under native fbclient");

    let rows = match result {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Affected { .. } => panic!("expected rows"),
    };
    assert_eq!(rows.len(), 1);
    let answer = &rows[0].cells[0];
    let name = &rows[0].cells[1];
    assert!(
        matches!(answer, ColumnValue::Integer(42)),
        "unexpected ANSWER cell: {answer:?}",
    );
    assert!(
        matches!(name, ColumnValue::Text(s) if s == "plamenix"),
        "unexpected NAME cell: {name:?}",
    );

    driver.close(session).await.expect("close");
}
