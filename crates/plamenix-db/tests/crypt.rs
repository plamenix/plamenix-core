//! Live integration tests for [`CryptState`] and the
//! `encryption_required` enforcement path.
//!
//! Requires the FB 5 Docker container described in
//! `plamenix/docs/firebird-driver.md`, on `127.0.0.1:3050` with the
//! seeded `test.fdb` (unencrypted). The DYLD setup from
//! `tests/native_tz.rs` applies if running with `--features native`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_db::{ConnectMode, ConnectionConfig, CryptState, DbDriver, DbError, RsfbDriver};

const HOST: &str = "127.0.0.1";
const PORT: u16 = 3050;
const DB_PATH: &str = "/var/lib/firebird/data/test.fdb";

fn pure_rust_config(encryption_required: bool) -> ConnectionConfig {
    ConnectionConfig {
        host: HOST.into(),
        port: PORT,
        database: DB_PATH.into(),
        user: "SYSDBA".into(),
        password: "masterkey".into(),
        encryption_key: None,
        fbclient_path: None,
        encryption_required,
        charset: None,
        embedded: false,
    }
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn crypt_state_returns_unencrypted_for_test_db() {
    let driver = RsfbDriver::new();
    let session = driver
        .connect(pure_rust_config(false), ConnectMode::PureRust)
        .await
        .expect("connect");

    let state = driver.crypt_state(session).await.expect("crypt_state");
    assert_eq!(state, CryptState::Unencrypted);

    driver.close(session).await.unwrap();
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn encryption_required_rejects_unencrypted_database() {
    let driver = RsfbDriver::new();
    let result = driver
        .connect(pure_rust_config(true), ConnectMode::PureRust)
        .await;
    assert!(
        matches!(&result, Err(DbError::Connect(msg)) if msg.contains("encryption_required")),
        "expected encryption_required to reject, got {result:?}",
    );
}
