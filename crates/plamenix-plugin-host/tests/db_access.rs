//! Can a plugin actually read the database?
//!
//! Every layer of this was built and tested separately: the world
//! linking, the capability gate, the service boundary, the type
//! mapping. This is the first thing that crosses all of them, with a
//! real wasm component on one end.
//!
//! The plugin logs what it read, and the assertions are on that log.
//! Asserting that the call returned `Ok` would pass against a plugin
//! that ignored every row, and asserting on the host's side of the
//! boundary would not prove the value survived the trip through WIT.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use plamenix_plugin_host::{
    CallCtx, EventBus, HostCell, HostColumn, HostQueryResult, HostRow, HostServices, HostState,
    InstanceRegistry, LogSink, PluginHost, ServiceError, activate_into_registry, dispatch_event,
    load,
};
use semver::Version;

const DB_READER_WASM: &[u8] = include_bytes!("fixtures/db-reader-plugin.wasm");
const PLUGIN_ID: &str = "dev.plamenix.dbreader";
const SESSION: &str = "11111111-2222-3333-4444-555555555555";

/// A stand-in for the shell's database.
///
/// Answers with one exact decimal — seventeen significant digits, more
/// than an `f64` can carry — because the value surviving is the point.
struct FakeShell {
    grants: Vec<String>,
    seen_sql: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl HostServices for FakeShell {
    fn granted_for(&self, _: &str) -> HashSet<String> {
        self.grants.iter().cloned().collect()
    }

    async fn db_query_read(
        &self,
        ctx: &CallCtx,
        sql: &str,
        _: u32,
    ) -> Result<HostQueryResult, ServiceError> {
        // The host should already have refused a session the plugin
        // invented, so anything arriving here is the one we set.
        assert_eq!(ctx.session_id.as_deref(), Some(SESSION));
        self.seen_sql.lock().unwrap().push(sql.to_owned());
        Ok(HostQueryResult {
            columns: vec![HostColumn {
                name: "AMOUNT".to_owned(),
                sql_type: None,
                nullable: None,
            }],
            rows: vec![HostRow {
                cells: vec![HostCell::Decimal("12345678901234567.89".to_owned())],
            }],
            truncated: false,
        })
    }
}

fn stage(dir: &std::path::Path, world: &str, permissions: &str) {
    std::fs::write(dir.join("plugin.wasm"), DB_READER_WASM).unwrap();
    std::fs::write(
        dir.join("manifest.toml"),
        format!(
            r#"
[plugin]
id = "{PLUGIN_ID}"
name = "DB Reader Fixture"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"
world = "plamenix:plugin@1.0.0/{world}"

[entry_points]
wasm = "plugin.wasm"

{permissions}
"#
        ),
    )
    .unwrap();
}

/// Activates the fixture and returns its log sink plus the fake shell.
async fn run(
    world: &str,
    permissions: &str,
    grants: Vec<String>,
    session: Option<&str>,
) -> (Vec<String>, Arc<FakeShell>) {
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path(), world, permissions);

    let shell = Arc::new(FakeShell {
        grants,
        seen_sql: Mutex::new(Vec::new()),
    });
    let sink: LogSink = Arc::new(Mutex::new(Vec::new()));
    let slot: plamenix_plugin_host::SessionSlot =
        Arc::new(Mutex::new(session.map(ToOwned::to_owned)));

    let staged = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path()).expect("load");
    let state = HostState::new(PLUGIN_ID, "1.0.0-beta")
        .with_log_sink(Arc::clone(&sink))
        .with_services(Arc::clone(&shell) as Arc<dyn HostServices>)
        .with_world(staged.manifest.plugin.world_tier)
        .with_declared_permissions(staged.manifest.permissions.clone())
        .with_session_slot(slot);
    activate_into_registry(&host, state, &staged, &registry)
        .await
        .expect("activate");

    let bus = EventBus::new();
    bus.subscribe(PLUGIN_ID, "**").expect("subscribe");
    dispatch_event(&bus, &registry, "db/query/executed", "{}").await;

    let lines = sink
        .lock()
        .unwrap()
        .iter()
        .map(|entry| entry.message.clone())
        .collect();
    (lines, shell)
}

const DB_PERMISSIONS: &str = r#"[permissions]
required = ["db.read.any", "db.session.context.read"]"#;

#[tokio::test]
async fn a_plugin_reads_a_row_and_gets_it_intact() {
    let (lines, shell) = run(
        "plugin-db-reader",
        DB_PERMISSIONS,
        vec![
            "db.read.any".to_owned(),
            "db.session.context.read".to_owned(),
        ],
        Some(SESSION),
    )
    .await;

    assert_eq!(
        shell.seen_sql.lock().unwrap().as_slice(),
        ["SELECT COUNT(*) FROM RDB$RELATIONS WHERE RDB$SYSTEM_FLAG = 0"],
        "the plugin's SQL should reach the shell verbatim",
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("db-reader read 1 row(s):")),
        "the plugin did not see the row: {lines:?}",
    );
    // The whole reason for the mirror types and the `decimal` arm. A
    // host that routed NUMERIC through a double would print
    // `float(12345678901234568)` here.
    assert!(
        lines
            .iter()
            .any(|line| line.contains("decimal(12345678901234567.89)")),
        "NUMERIC did not survive the trip to the plugin: {lines:?}",
    );
}

#[tokio::test]
async fn the_same_plugin_is_refused_when_the_user_granted_nothing() {
    // The manifest is identical. Only the user's decision differs, and
    // that is what has to be load-bearing.
    let (lines, shell) = run(
        "plugin-db-reader",
        DB_PERMISSIONS,
        Vec::new(),
        Some(SESSION),
    )
    .await;

    assert!(
        shell.seen_sql.lock().unwrap().is_empty(),
        "an ungranted plugin must never reach the database",
    );
    assert!(
        lines.iter().any(|line| line.contains("refused")),
        "the plugin should have been told: {lines:?}",
    );
}

#[tokio::test]
async fn a_plugin_with_no_session_reads_nothing() {
    let (_, shell) = run(
        "plugin-db-reader",
        DB_PERMISSIONS,
        vec![
            "db.read.any".to_owned(),
            "db.session.context.read".to_owned(),
        ],
        None,
    )
    .await;

    assert!(
        shell.seen_sql.lock().unwrap().is_empty(),
        "with no session there is no database to read",
    );
}
