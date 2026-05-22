//! Offline smoke tests for `plamenix-db`.
//!
//! These do not touch a real Firebird server; they verify that the
//! driver's surface compiles, types resolve, and error paths return
//! the expected variants. Live integration tests against a real
//! Firebird instance live elsewhere (CI matrix with Docker).

use plamenix_db::{ConnectMode, DbDriver, DbError, RsfbDriver};
use plamenix_types::{ConnectionConfig, SessionId};

fn fake_config() -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: 65535,
        database: "/does/not/exist.fdb".into(),
        user: "SYSDBA".into(),
        password: "masterkey".into(),
        encryption_key: None,
        fbclient_path: None,
        encryption_required: false,
        charset: None,
    }
}

#[tokio::test]
async fn connect_to_unreachable_host_yields_connect_error() {
    let driver = RsfbDriver::new();
    let result = driver.connect(fake_config(), ConnectMode::PureRust).await;
    assert!(
        matches!(result, Err(DbError::Connect(_))),
        "expected DbError::Connect, got {result:?}",
    );
}

#[tokio::test]
async fn close_unknown_session_yields_driver_error() {
    let driver = RsfbDriver::new();
    let result = driver.close(SessionId::new()).await;
    assert!(
        matches!(result, Err(DbError::Driver(_))),
        "expected DbError::Driver, got {result:?}",
    );
}

#[tokio::test]
async fn execute_unknown_session_yields_driver_error() {
    let driver = RsfbDriver::new();
    let result = driver
        .execute(SessionId::new(), "SELECT 1 FROM rdb$database".into())
        .await;
    assert!(
        matches!(result, Err(DbError::Driver(_))),
        "expected DbError::Driver, got {result:?}",
    );
}
