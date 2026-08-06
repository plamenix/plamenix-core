//! Guards against a Plamenix session pinning the oldest active
//! transaction.
//!
//! Firebird reclaims old record versions only up to the oldest active
//! transaction, so a session that never releases one stalls garbage
//! collection for the whole database. This measures it through the real
//! driver rather than reasoning about the client library — which
//! matters, because reasoning about it got the wrong answer:
//! `rsfbclient` commits via `commit_retaining` on every path, including
//! plain autocommit, which reads like a guaranteed pin. Measured
//! against Firebird 5.0.4 the gap stays at 1, so it does not manifest.
//!
//! Two things to know before extending this file. Firebird's `MON$`
//! tables are snapshot-stable: their contents are captured when a
//! transaction first reads them and stay frozen for that transaction's
//! lifetime, so any observer that reuses a connection reports the same
//! numbers forever — each reading needs a fresh attachment. And every
//! observing connection is itself an active transaction, so it counts
//! toward whatever it measures.
//!
//! The guard matters most once manual transaction mode lands: holding a
//! transaction open is that feature's entire point, and this is what
//! catches it leaking into the read path.
//!
//! ```sh
//! cargo test -p plamenix-db --test oat_probe -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_db::{
    ColumnValue, ConnectMode, ConnectionConfig, DbDriver, QueryResult, RsfbDriver, SessionId,
};

fn config() -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port: 3050,
        database: "/var/lib/firebird/data/test.fdb".into(),
        user: "SYSDBA".into(),
        password: "masterkey".into(),
        encryption_key: None,
        fbclient_path: None,
        encryption_required: false,
        charset: None,
        embedded: false,
    }
}

async fn scalar(driver: &RsfbDriver, session: SessionId, sql: &str) -> i64 {
    let result = driver
        .execute(session, sql.to_string())
        .await
        .expect("query");
    match result {
        QueryResult::Rows { rows, .. } => match rows[0].cells[0] {
            ColumnValue::Integer(v) => v,
            ref other => panic!("expected integer, got {other:?}"),
        },
        QueryResult::Affected { .. } => panic!("expected rows"),
    }
}

#[tokio::test]
#[ignore = "needs running Firebird"]
async fn a_session_should_not_pin_the_oldest_active_transaction() {
    let driver = RsfbDriver::new();
    let observer = driver
        .connect(config(), ConnectMode::PureRust)
        .await
        .expect("observer session");
    let worker = driver
        .connect(config(), ConnectMode::PureRust)
        .await
        .expect("worker session");

    // Give the worker something to do, then see whether the transaction
    // it used is still registered as active.
    for _ in 0..5 {
        let _ = scalar(&driver, worker, "SELECT 1 FROM RDB$DATABASE").await;
    }

    let oat = scalar(
        &driver,
        observer,
        "SELECT MON$OLDEST_ACTIVE FROM MON$DATABASE",
    )
    .await;
    let next = scalar(
        &driver,
        observer,
        "SELECT MON$NEXT_TRANSACTION FROM MON$DATABASE",
    )
    .await;
    let active = scalar(
        &driver,
        observer,
        "SELECT COUNT(*) FROM MON$TRANSACTIONS WHERE MON$STATE = 1",
    )
    .await;

    println!(
        "oldest_active={oat} next={next} gap={} active_tx={active}",
        next - oat
    );

    // Two sessions are open, so two transactions may legitimately be
    // active. A gap that keeps pace with the transaction counter is the
    // symptom of a session that never releases one.
    assert!(
        next - oat <= 4,
        "a Plamenix session is pinning the oldest active transaction: \
         oldest_active={oat}, next={next}, gap={}. Firebird cannot \
         garbage-collect record versions past the oldest active \
         transaction, so this stalls sweep for the whole database.",
        next - oat,
    );

    driver.close(worker).await.expect("close worker");
    driver.close(observer).await.expect("close observer");
}
