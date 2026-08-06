//! Live tests for the Firebird 2.5 code paths.
//!
//! Plamenix documents support for Firebird 2.5 through 5.0, and two
//! monitoring columns only exist from 3.0 onward. Selecting one on 2.5
//! does not degrade a field — it fails the whole query with "Column
//! unknown", taking the dashboard down with it.
//!
//! Point these at either engine:
//!
//! ```sh
//! cargo test -p plamenix-db --test version_gating -- --ignored
//! PLAMENIX_TEST_PORT=3051 PLAMENIX_TEST_DB=/firebird/data/test.fdb \
//!   cargo test -p plamenix-db --test version_gating -- --ignored
//! ```
//!
//! See plamenix/dev/README.md for the containers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_db::{ConnectMode, ConnectionConfig, CryptState, DbDriver, RsfbDriver};

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
        fbclient_path: None,
        encryption_required: false,
        charset: None,
        embedded: false,
    }
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn the_dashboard_loads_on_every_supported_engine() {
    let driver = RsfbDriver::new();
    let session = driver
        .connect(config(), ConnectMode::PureRust)
        .await
        .expect("connect");

    let stats = driver
        .database_stats(session)
        .await
        .expect("database_stats must work across the whole supported range");

    // Fields present on every supported version should be populated,
    // proving the gating substituted a column rather than losing the row.
    assert!(!stats.database.name.is_empty());
    assert!(stats.database.page_size > 0);
    assert!(!stats.database.server_version.is_empty());

    let major: u32 = stats
        .database
        .server_version
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .expect("engine version should start with a major number");

    if major >= 3 {
        assert!(
            !stats.database.owner.is_empty(),
            "MON$OWNER exists from 3.0 and should be reported",
        );
    } else {
        // 2.5 has no MON$OWNER at all; empty is the honest answer.
        assert!(stats.database.owner.is_empty());
    }

    driver.close(session).await.expect("close");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn crypt_state_answers_on_every_supported_engine() {
    let driver = RsfbDriver::new();
    let session = driver
        .connect(config(), ConnectMode::PureRust)
        .await
        .expect("connect");

    // On 2.5 this is answered without touching MON$CRYPT_STATE, which
    // does not exist there — and unencrypted is factual, since that
    // engine has no native database encryption at all.
    let state = driver
        .crypt_state(session)
        .await
        .expect("crypt_state must work across the whole supported range");
    assert_eq!(
        state,
        CryptState::Unencrypted,
        "the dev database is not encrypted"
    );

    driver.close(session).await.expect("close");
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn schema_and_queries_work_on_every_supported_engine() {
    // Guards the rest of the surface the dashboard depends on, so a 2.5
    // regression shows up here rather than in a user's session.
    let driver = RsfbDriver::new();
    let session = driver
        .connect(config(), ConnectMode::PureRust)
        .await
        .expect("connect");

    let schema = driver
        .describe_schema(session)
        .await
        .expect("describe_schema");
    assert!(
        !schema.tables.is_empty(),
        "the seeded dev schema should report tables",
    );

    let version = driver.ping(session).await.expect("ping");
    assert!(!version.is_empty());

    driver.close(session).await.expect("close");
}
