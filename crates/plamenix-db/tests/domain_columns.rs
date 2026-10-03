//! Domain-based columns, against a real engine.
//!
//! **One live suite at a time** — see `transactions.rs`.
//!
//! A column created as `"userUuid" D_UUID_DDL NOT NULL` must describe with
//! `domain = D_UUID_DDL` while `sql_type` keeps the resolved base type
//! (`CHAR(16)`): DDL regeneration re-declares the domain
//! (plamenix-desktop#4) and type-aware behaviour keeps reading the
//! base type. Expanding the domain into the DDL — the old behaviour —
//! silently detached the column from its domain on a re-run.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_db::{ConnectMode, ConnectionConfig, DbDriver, RsfbDriver, SessionId};

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

/// Runs DDL with the same retry-and-insist discipline as
/// `schema_types.rs`: a swallowed failure here fails an assertion
/// several lines later, on something unrelated.
async fn run(driver: &RsfbDriver, session: SessionId, sql: &str) {
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

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn a_domain_column_describes_with_its_domain_and_base_type() {
    let driver = RsfbDriver::new();
    let session = driver
        .connect(config(), ConnectMode::PureRust)
        .await
        .expect("connect");

    // Idempotent from run to run: the schema persists in the shared
    // test database between invocations. Best-effort drops — on a
    // fresh database both have nothing to drop.
    let _ = driver
        .execute(session, "DROP TABLE \"tblDomainDdlTest\";".into())
        .await;
    let _ = driver
        .execute(session, "DROP DOMAIN D_UUID_DDL;".into())
        .await;
    let _ = driver
        .execute(session, "DROP DOMAIN D_MONEY_DDL;".into())
        .await;

    // `BINARY(16)` needs FB 4+; the explicit OCTETS form is identical
    // on the wire and accepted everywhere, so prefer it for portability
    // and let a BINARY domain be the second, engine-dependent case.
    run(
        &driver,
        session,
        "CREATE DOMAIN D_UUID_DDL AS CHAR(16) CHARACTER SET OCTETS;",
    )
    .await;
    run(&driver, session, "CREATE DOMAIN D_MONEY_DDL AS NUMERIC(18,4);").await;
    run(
        &driver,
        session,
        "CREATE TABLE \"tblDomainDdlTest\" (\
             \"userUuid\" D_UUID_DDL NOT NULL, \
             AMOUNT D_MONEY_DDL, \
             NOTE VARCHAR(100));",
    )
    .await;

    let schema = driver.describe_schema(session).await.expect("describe");
    let table = schema
        .tables
        .iter()
        .find(|t| t.name == "tblDomainDdlTest")
        .expect("table in schema");

    let uuid = table.columns.iter().find(|c| c.name == "userUuid").expect("column");
    assert_eq!(uuid.domain.as_deref(), Some("D_UUID_DDL"), "domain name surfaced");
    assert_eq!(uuid.sql_type, "CHAR(16)", "sql_type stays the base type");
    assert!(!uuid.nullable, "NOT NULL rides on the column, not the domain");

    let amount = table.columns.iter().find(|c| c.name == "AMOUNT").expect("column");
    assert_eq!(amount.domain.as_deref(), Some("D_MONEY_DDL"));
    assert_eq!(amount.sql_type, "NUMERIC(18,4)");

    let note = table.columns.iter().find(|c| c.name == "NOTE").expect("column");
    assert_eq!(note.domain, None, "inline-typed column has no domain");
    assert_eq!(note.sql_type, "VARCHAR(100)");

    driver.close(session).await.expect("close");
}
