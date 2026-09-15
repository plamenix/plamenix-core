//! Functions in the described schema, against a real engine.
//!
//! **One live suite at a time** — see `transactions.rs`.
//!
//! `RDB$FUNCTIONS` holds legacy external functions (UDFs) and, on
//! Firebird 3+, PSQL stored functions; neither surfaced anywhere in
//! the schema before, so the object tree had no way to show them
//! (plamenix-desktop#6). This test pins that `describe_schema`
//! reports declared functions with the UDF/PSQL split derived from
//! `RDB$MODULE_NAME`.
//!
//! Engine coverage is adaptive: PSQL functions exist from Firebird 3.0
//! (core SQL — any FB 3+ server runs the PSQL branch), while UDF
//! declarations depend on the server's `UDFAccess` setting (default
//! installations of FB 3+ refuse them; 2.5 allows them). Whichever
//! branch the engine supports is asserted; a server that supports
//! neither fails the test.
//!
//! ```sh
//! cargo test -p plamenix-db --test schema_functions -- --ignored
//! PLAMENIX_TEST_PORT=3050 PLAMENIX_TEST_DB=/firebird/data/test.fdb \
//!   cargo test -p plamenix-db --test schema_functions -- --ignored
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_db::{ConnectMode, ConnectionConfig, DbDriver, RsfbDriver};

fn config() -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: std::env::var("PLAMENIX_TEST_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3050),
        database: std::env::var("PLAMENIX_TEST_DB")
            .unwrap_or_else(|_| "/var/lib/firebird/data/test.fdb".into()),
        user: "SYSDBA".into(),
        password: "masterkey".into(),
        encryption_key: None,
        fbclient_path: std::env::var("PLAMENIX_FBCLIENT_PATH").ok(),
        encryption_required: false,
        charset: None,
        embedded: false,
    }
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn declared_functions_surface_with_their_kind() {
    let driver = RsfbDriver::new();
    let session = driver
        .connect(config(), ConnectMode::PureRust)
        .await
        .expect("connect");

    // Firebird does not resolve the module at DECLARE time (only at
    // call time), so a module that does not exist is fine — the
    // function still lands in RDB$FUNCTIONS as a UDF. Refused when the
    // server has UDFs disabled (FB 3+ default `UDFAccess`), which this
    // test then simply does not exercise.
    let udf_declared = driver
        .execute(
            session,
            "DECLARE EXTERNAL FUNCTION PLAMENIX_TEST_UDF \
             RETURNS INTEGER BY VALUE \
             ENTRY_POINT 'plamenix_never_called' MODULE_NAME 'plamenix_missing_udf';"
                .into(),
        )
        .await
        .is_ok();

    // PSQL stored functions exist from Firebird 3.0; on 2.5 the
    // statement is a syntax error and the branch is skipped.
    let psql_declared = driver
        .execute(
            session,
            "CREATE FUNCTION PLAMENIX_TEST_PSQLF(A INTEGER) RETURNS INTEGER \
             AS BEGIN RETURN A + 1; END;"
                .into(),
        )
        .await
        .is_ok();

    assert!(
        udf_declared || psql_declared,
        "the test engine supports neither UDF declarations nor PSQL \
         functions — point PLAMENIX_TEST_* at a usable server"
    );

    let schema = driver.describe_schema(session).await.expect("describe");
    if udf_declared {
        let udf = schema
            .functions
            .iter()
            .find(|f| f.name == "PLAMENIX_TEST_UDF")
            .expect("declared UDF should be in the described schema");
        assert!(udf.udf, "a module-backed function is a UDF");
    }
    if psql_declared {
        let func = schema
            .functions
            .iter()
            .find(|f| f.name == "PLAMENIX_TEST_PSQLF")
            .expect("PSQL function should be in the described schema");
        assert!(!func.udf, "a function with no module is PSQL, not a UDF");
    }

    driver.close(session).await.expect("close");
}
