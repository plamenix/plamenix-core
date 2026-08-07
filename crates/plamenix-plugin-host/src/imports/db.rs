//! `db` and `db-write` — a plugin talking to the user's database.
//!
//! ## The session parameter is an assertion, not a selector
//!
//! Every function here takes a `session-id`, which reads like the
//! plugin choosing which database to talk to. It is not, and treating
//! it that way would be a cross-tenant read on the web edition, where
//! one plugin instance serves every user of the server. The host
//! already knows which session it made the call for; the supplied id is
//! checked against that and a mismatch is refused.
//!
//! ## Writes do not run the interceptor chain
//!
//! `plugin-interceptors.md` used to promise that `db-write.execute`
//! would run `query.executing` first. It cannot. An interceptor is
//! called with the plugin's store locked, so a plugin that both
//! intercepts the chain and calls `execute` would deadlock itself, and
//! two such plugins would deadlock each other. The chain governs
//! statements the *user* originated. What governs plugin SQL is the
//! capability check below, which is the thing the user actually
//! approved.

use crate::bindings::plamenix::plugin::db::{
    BlobRef, ColumnInfo, ColumnValue, DbError, Host as DbHost, QueryResult, Row, Schema, TableInfo,
};
use crate::bindings::plamenix::plugin::db_write::{
    ExecResult, Host as DbWriteHost, StatementOutcome,
};
use crate::capability::Permission;
use crate::gate::{self, Guard};
use crate::host_impl::HostState;
use crate::imports::{DB_IMPORT_BUDGET, PLUGIN_ROW_CAP, bounded};
use crate::services::{HostCell, HostQueryResult, HostStatementOutcome, ServiceError};

/// Reading a row needs read access; holding write access implies it.
const READ_GUARD: Guard = Guard::Any(&[Permission::DbReadAny, Permission::DbWriteAny]);
/// The catalogue is its own capability pair.
const SCHEMA_GUARD: Guard = Guard::Any(&[Permission::DbSchemaList, Permission::DbSchemaDescribe]);
/// Writing needs write access. DDL is a separate grant the driver
/// enforces at the statement level; both let a plugin call `execute`.
const WRITE_GUARD: Guard = Guard::Any(&[Permission::DbWriteAny, Permission::DbDdlAny]);
/// Knowing which session the host is acting for.
const SESSION_GUARD: Guard = Guard::Any(&[Permission::DbSessionContextRead]);

impl From<ServiceError> for DbError {
    fn from(err: ServiceError) -> Self {
        match err {
            ServiceError::NoSession => Self::NoSession,
            ServiceError::Unsupported(what) => {
                Self::CapabilityDenied(format!("{what} is not available in this edition"))
            }
            other => Self::Execution(other.to_string()),
        }
    }
}

/// Confirms the plugin is asking about the session the host called it
/// for.
fn resolve_session(state: &HostState, supplied: &str) -> Result<(), DbError> {
    match state.current_session() {
        None => Err(DbError::NoSession),
        Some(current) if current == supplied => Ok(()),
        Some(_) => Err(DbError::InvalidSession(
            "session-id does not match the session this call was made for".to_owned(),
        )),
    }
}

fn to_column(column: crate::services::HostColumn) -> ColumnInfo {
    ColumnInfo {
        name: column.name,
        sql_type: column.sql_type,
        nullable: column.nullable,
    }
}

/// The one place a host cell becomes a WIT value.
///
/// Kept in a single function on purpose. `Decimal` is the reason the
/// service layer has its own cell type at all: Firebird stores NUMERIC
/// as a scaled integer and this repo vendors a patched driver to stop
/// it being coerced to a double. If this mapping ever routes it through
/// `Float`, that work is undone for every plugin handling money — and
/// it would be undone silently.
fn to_value(cell: HostCell) -> ColumnValue {
    match cell {
        HostCell::Null => ColumnValue::Null,
        HostCell::Text(text) => ColumnValue::Text(text),
        HostCell::Integer(value) => ColumnValue::Integer(value),
        HostCell::Decimal(text) => ColumnValue::Decimal(text),
        HostCell::Float(value) => ColumnValue::Float(value),
        HostCell::Bool(value) => ColumnValue::Boolean(value),
        HostCell::Blob { id, size, peek_hex } => {
            ColumnValue::BlobRef(BlobRef { id, size, peek_hex })
        }
    }
}

fn to_result(result: HostQueryResult) -> QueryResult {
    QueryResult {
        columns: result.columns.into_iter().map(to_column).collect(),
        rows: result
            .rows
            .into_iter()
            .map(|row| Row {
                cells: row.cells.into_iter().map(to_value).collect(),
            })
            .collect(),
        truncated: result.truncated,
    }
}

#[async_trait::async_trait]
impl DbHost for HostState {
    async fn describe_schema(
        &mut self,
        session_id: String,
    ) -> wasmtime::Result<Result<Schema, DbError>> {
        if let Err(denial) = gate::check(self, &SCHEMA_GUARD) {
            return Ok(Err(DbError::CapabilityDenied(denial.to_string())));
        }
        if let Err(err) = resolve_session(self, &session_id) {
            return Ok(Err(err));
        }

        let services = std::sync::Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let outcome = bounded(self, DB_IMPORT_BUDGET, services.db_describe_schema(&ctx)).await;

        Ok(outcome
            .map(|schema| Schema {
                tables: schema
                    .tables
                    .into_iter()
                    .map(|table| TableInfo {
                        name: table.name,
                        kind: table.kind,
                    })
                    .collect(),
            })
            .map_err(DbError::from))
    }

    async fn current_session(&mut self) -> wasmtime::Result<Result<Option<String>, DbError>> {
        if let Err(denial) = gate::check(self, &SESSION_GUARD) {
            return Ok(Err(DbError::CapabilityDenied(denial.to_string())));
        }
        // `ok(none)` is "the user is on the connect screen", which is
        // why a denial above had to be an `err` and not the same value.
        //
        // Read through `call_ctx` rather than the inherent accessor:
        // this WIT import and that accessor share a name, and inside
        // the trait impl the trait's method wins.
        Ok(Ok(self.call_ctx().session_id))
    }

    async fn query_read(
        &mut self,
        session_id: String,
        sql: String,
    ) -> wasmtime::Result<Result<QueryResult, DbError>> {
        if let Err(denial) = gate::check(self, &READ_GUARD) {
            return Ok(Err(DbError::CapabilityDenied(denial.to_string())));
        }
        if let Err(err) = resolve_session(self, &session_id) {
            return Ok(Err(err));
        }

        let services = std::sync::Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let outcome = bounded(
            self,
            DB_IMPORT_BUDGET,
            services.db_query_read(&ctx, &sql, PLUGIN_ROW_CAP),
        )
        .await;

        Ok(outcome.map(to_result).map_err(DbError::from))
    }
}

#[async_trait::async_trait]
impl DbWriteHost for HostState {
    async fn execute(
        &mut self,
        session_id: String,
        sql: String,
    ) -> wasmtime::Result<Result<StatementOutcome, DbError>> {
        if let Err(denial) = gate::check(self, &WRITE_GUARD) {
            return Ok(Err(DbError::CapabilityDenied(denial.to_string())));
        }
        if let Err(err) = resolve_session(self, &session_id) {
            return Ok(Err(err));
        }

        let services = std::sync::Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let outcome = bounded(self, DB_IMPORT_BUDGET, services.db_execute(&ctx, &sql)).await;

        Ok(outcome
            .map(|outcome| match outcome {
                HostStatementOutcome::Rows(rows) => StatementOutcome::Rows(to_result(rows)),
                HostStatementOutcome::Affected(rows) => StatementOutcome::Affected(ExecResult {
                    affected_rows: rows,
                }),
            })
            .map_err(DbError::from))
    }

    async fn tx_begin(&mut self, session_id: String) -> wasmtime::Result<Result<(), DbError>> {
        Ok(self.tx(session_id, TxOp::Begin).await)
    }

    async fn tx_commit(&mut self, session_id: String) -> wasmtime::Result<Result<(), DbError>> {
        Ok(self.tx(session_id, TxOp::Commit).await)
    }

    async fn tx_rollback(&mut self, session_id: String) -> wasmtime::Result<Result<(), DbError>> {
        Ok(self.tx(session_id, TxOp::Rollback).await)
    }
}

/// Which end of a transaction is being asked for.
#[derive(Clone, Copy)]
enum TxOp {
    Begin,
    Commit,
    Rollback,
}

impl HostState {
    /// The three transaction imports differ only in which driver call
    /// they make, so they share everything else.
    ///
    /// No capability of their own: a plugin holding `db.write.any` can
    /// already change the data, and a separate `db.tx.*` grant would
    /// put a second prompt in front of something the first one implies.
    async fn tx(&mut self, session_id: String, op: TxOp) -> Result<(), DbError> {
        if let Err(denial) = gate::check(self, &WRITE_GUARD) {
            return Err(DbError::CapabilityDenied(denial.to_string()));
        }
        resolve_session(self, &session_id)?;

        let services = std::sync::Arc::clone(&self.services);
        let ctx = self.call_ctx();
        let future = async move {
            match op {
                TxOp::Begin => services.db_tx_begin(&ctx).await,
                TxOp::Commit => services.db_tx_commit(&ctx).await,
                TxOp::Rollback => services.db_tx_rollback(&ctx).await,
            }
        };
        bounded(self, DB_IMPORT_BUDGET, future)
            .await
            .map_err(DbError::from)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::capability::{PermissionGrant, PermissionSet};
    use crate::services::{CallCtx, HostColumn, HostRow, HostServices};
    use crate::world::PluginWorld;

    /// Records the SQL it was asked to run and answers with one exact
    /// decimal, which is the value the whole mirror-type design exists
    /// to carry intact.
    #[derive(Default)]
    struct FakeDb {
        seen: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl HostServices for FakeDb {
        fn granted_for(&self, _: &str) -> HashSet<String> {
            HashSet::from([
                "db.read.any".to_owned(),
                "db.write.any".to_owned(),
                "db.session.context.read".to_owned(),
            ])
        }
        async fn db_query_read(
            &self,
            _: &CallCtx,
            sql: &str,
            _: u32,
        ) -> Result<HostQueryResult, ServiceError> {
            self.seen.lock().unwrap().push(sql.to_owned());
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

    fn state(services: Arc<dyn HostServices>, session: Option<&str>) -> HostState {
        let declared = [
            Permission::DbReadAny,
            Permission::DbWriteAny,
            Permission::DbSessionContextRead,
        ];
        let mut host = HostState::new("dev.plamenix.test", "1.0.0-beta")
            .with_world(PluginWorld::DbWriter)
            .with_declared_permissions(PermissionSet {
                required: declared.into_iter().map(PermissionGrant::new).collect(),
                optional: Vec::new(),
            })
            .with_services(services);
        if let Some(session) = session {
            *host.session.lock().unwrap() = Some(session.to_owned());
        }
        host.refresh_grants();
        host
    }

    #[tokio::test]
    async fn an_exact_decimal_survives_the_trip_to_the_plugin() {
        // Seventeen significant digits: a double would have lost the
        // tail, and losing it silently is what the vendored driver
        // patch exists to prevent.
        let mut host = state(Arc::new(FakeDb::default()), Some("s1"));
        let result = host
            .query_read("s1".to_owned(), "SELECT amount FROM ledger".to_owned())
            .await
            .unwrap()
            .unwrap();

        match &result.rows[0].cells[0] {
            ColumnValue::Decimal(text) => assert_eq!(text, "12345678901234567.89"),
            other => panic!("NUMERIC must not arrive as anything else: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_plugin_cannot_name_a_session_the_host_did_not_call_it_for() {
        // The cross-tenant read. On the web edition one plugin instance
        // serves every user, so a plugin that could choose its own
        // session could read a database the caller never opened.
        let mut host = state(Arc::new(FakeDb::default()), Some("mine"));
        let refused = host
            .query_read("someone-elses".to_owned(), "SELECT 1".to_owned())
            .await
            .unwrap();

        assert!(matches!(refused, Err(DbError::InvalidSession(_))));
    }

    #[tokio::test]
    async fn no_session_at_all_is_its_own_answer() {
        let mut host = state(Arc::new(FakeDb::default()), None);
        let refused = host
            .query_read("anything".to_owned(), "SELECT 1".to_owned())
            .await
            .unwrap();

        assert!(matches!(refused, Err(DbError::NoSession)));
    }

    #[tokio::test]
    async fn an_ungranted_plugin_is_refused_rather_than_trapped() {
        // The outer Result must stay Ok. An Err there raises a guest
        // trap, and the supervisor would charge crash budget for the
        // host's own policy decision.
        struct GrantsNothing;
        #[async_trait::async_trait]
        impl HostServices for GrantsNothing {
            fn granted_for(&self, _: &str) -> HashSet<String> {
                HashSet::new()
            }
        }

        let mut host = state(Arc::new(GrantsNothing), Some("s1"));
        let outer = host
            .query_read("s1".to_owned(), "SELECT 1".to_owned())
            .await;

        let refused = outer.expect("a denial must never trap the plugin");
        assert!(matches!(refused, Err(DbError::CapabilityDenied(_))));
    }

    #[tokio::test]
    async fn an_edition_without_a_database_says_so() {
        struct NoDb;
        #[async_trait::async_trait]
        impl HostServices for NoDb {
            fn granted_for(&self, _: &str) -> HashSet<String> {
                HashSet::from(["db.read.any".to_owned()])
            }
        }

        let mut host = state(Arc::new(NoDb), Some("s1"));
        let refused = host
            .query_read("s1".to_owned(), "SELECT 1".to_owned())
            .await
            .unwrap();

        match refused {
            Err(DbError::CapabilityDenied(message)) => {
                assert!(message.contains("db"), "unhelpful message: {message}");
            }
            other => panic!("expected a capability denial, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn waiting_on_the_host_is_repaid_to_the_plugins_budget() {
        // Without this the plugin is preempted for our I/O latency and
        // the supervisor charges it crash budget for waiting on our
        // database.
        struct SlowDb;
        #[async_trait::async_trait]
        impl HostServices for SlowDb {
            fn granted_for(&self, _: &str) -> HashSet<String> {
                HashSet::from(["db.read.any".to_owned()])
            }
            async fn db_query_read(
                &self,
                _: &CallCtx,
                _: &str,
                _: u32,
            ) -> Result<HostQueryResult, ServiceError> {
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                Ok(HostQueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    truncated: false,
                })
            }
        }

        let mut host = state(Arc::new(SlowDb), Some("s1"));
        assert_eq!(host.host_import_ticks, 0);
        let _ = host
            .query_read("s1".to_owned(), "SELECT 1".to_owned())
            .await
            .unwrap();

        assert!(
            host.host_import_ticks >= 5,
            "60ms of host time should repay at least 5 ten-millisecond ticks, got {}",
            host.host_import_ticks,
        );
    }
}
