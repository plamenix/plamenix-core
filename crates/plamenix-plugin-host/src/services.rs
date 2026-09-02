//! The boundary between the plugin host and the shell embedding it.
//!
//! Most of what a plugin wants is something only the shell can give it.
//! The database lives behind a driver the shell constructed, the
//! keyring is an OS facility one edition has and the other does not,
//! and the clipboard belongs to a window. The plugin host cannot reach
//! any of it — it must not depend on Tauri, on Fastify, or on the
//! database stack — so the shell hands it in.
//!
//! One trait rather than six. It is low cohesion read as a list of
//! features and high cohesion read as what it is: everything the
//! embedding host provides on the plugin's behalf. Six `Arc` fields on
//! [`crate::host_impl::HostState`] would be the same coupling with more
//! threading.
//!
//! ## Every method but one has a default
//!
//! The default is [`ServiceError::Unsupported`]. The web edition has no
//! OS keyring, no clipboard, and no notification centre, and should not
//! have to write nine stubs saying so. It implements what it has.
//!
//! The exception is [`HostServices::granted_for`]. A host that cannot
//! say what the user approved cannot enforce anything, and defaulting
//! it to "nothing is granted" would make every plugin look broken while
//! the actual fault was the embedding.
//!
//! ## Why these types are not the generated ones
//!
//! The signatures below use local mirrors — [`HostQueryResult`],
//! [`HostCell`] — rather than the `bindgen!` output. Three reasons, in
//! increasing order of importance. It keeps `plamenix-db` and its
//! vendored rsfbclient FFI out of this crate. It keeps both shells off
//! a `#![allow(missing_docs, clippy::all)]` generated module whose
//! shape moves whenever the WIT does.
//!
//! And it keeps [`HostCell::Decimal`]. Firebird stores NUMERIC and
//! DECIMAL as scaled integers, and this repo vendors a patched
//! `rsfbclient-rust` precisely to stop them being coerced to a double
//! in the driver. If the shells converted straight to the WIT types,
//! that conversion would happen twice, out of sight, and the precision
//! would be gone before anyone noticed. Here it happens once, in
//! `host_impl`, where it can be read.

use std::collections::HashSet;

/// Who the host is acting for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallCtx {
    /// Manifest id of the calling plugin.
    pub plugin_id: String,
    /// The session the host made this call for.
    ///
    /// The host's view, never the plugin's claim. The `db` imports take
    /// a `session-id` parameter that reads like a selector; it is
    /// checked against this and a mismatch is refused. On the web
    /// edition one plugin instance serves every tenant, so a plugin
    /// that could name its own session could read a database the user
    /// never opened.
    pub session_id: Option<String>,
}

/// Why a shell-provided service could not do what was asked.
///
/// Not a capability decision — those are made in [`crate::gate`] before
/// a service is called at all, so that policy never depends on an
/// embedding getting it right.
#[derive(Clone, Debug, thiserror::Error, Eq, PartialEq)]
pub enum ServiceError {
    /// This edition does not provide the named facility.
    #[error("{0} is not available in this edition")]
    Unsupported(&'static str),
    /// The call needed a session and there is none.
    #[error("no active session")]
    NoSession,
    /// The named thing does not exist.
    #[error("{0} not found")]
    NotFound(String),
    /// The underlying facility failed.
    #[error("{0}")]
    Backend(String),
    /// The host bounded the call and it ran out of time.
    #[error("host call timed out")]
    TimedOut,
}

/// One column's descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostColumn {
    /// Column name as the engine reported it.
    pub name: String,
    /// SQL type name, when the driver knows it.
    pub sql_type: Option<String>,
    /// Nullability, when the driver knows it.
    pub nullable: Option<bool>,
}

/// One cell.
///
/// Mirrors `plamenix_db::ColumnValue`, including the
/// [`Decimal`](Self::Decimal) arm that keeps NUMERIC exact.
#[derive(Clone, Debug, PartialEq)]
pub enum HostCell {
    /// SQL NULL.
    Null,
    /// Character data.
    Text(String),
    /// Exact integer.
    Integer(i64),
    /// NUMERIC / DECIMAL as exact decimal text, never a float.
    Decimal(String),
    /// Approximate numeric.
    Float(f64),
    /// Boolean.
    Bool(bool),
    /// A blob the plugin can fetch separately; the body is not inlined.
    Blob {
        /// Opaque handle.
        id: String,
        /// Size in bytes.
        size: u64,
        /// Short hex preview.
        peek_hex: String,
    },
}

/// One row.
#[derive(Clone, Debug, PartialEq)]
pub struct HostRow {
    /// Cells, positionally matching the result's columns.
    pub cells: Vec<HostCell>,
}

/// A result set.
#[derive(Clone, Debug, PartialEq)]
pub struct HostQueryResult {
    /// Column descriptors.
    pub columns: Vec<HostColumn>,
    /// Rows.
    pub rows: Vec<HostRow>,
    /// Whether the host applied a row cap and stopped early.
    pub truncated: bool,
}

/// What a statement produced.
#[derive(Clone, Debug, PartialEq)]
pub enum HostStatementOutcome {
    /// It returned rows.
    Rows(HostQueryResult),
    /// It affected rows without returning any.
    Affected(u64),
}

/// A catalogue entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostTable {
    /// Object name.
    pub name: String,
    /// `"table"` or `"view"`.
    pub kind: String,
}

/// The catalogue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSchema {
    /// Tables and views.
    pub tables: Vec<HostTable>,
}

/// An outbound HTTP request.
///
/// The host has already decided the URL is allowed before this is
/// handed over — see [`crate::gate::check_net`]. The shell is the
/// transport and does not repeat the policy decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostHttpRequest {
    /// Method verb.
    pub method: String,
    /// Absolute URL.
    pub url: String,
    /// Header name/value pairs.
    pub headers: Vec<(String, String)>,
    /// Optional body.
    pub body: Option<Vec<u8>>,
}

/// An HTTP response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostHttpResponse {
    /// Status code.
    pub status: u16,
    /// Header name/value pairs.
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: Vec<u8>,
}

/// What the embedding shell provides on a plugin's behalf.
///
/// See the module docs for why this is one trait, why every method but
/// [`granted_for`](Self::granted_for) is defaulted, and why the
/// signatures avoid the generated types.
#[async_trait::async_trait]
pub trait HostServices: Send + Sync + 'static {
    /// Capability strings the user has approved for this plugin.
    ///
    /// The only method without a default, because a wrong answer here
    /// is silent: defaulting to an empty set would refuse every import
    /// and read as the plugin being broken.
    ///
    /// Called once per plugin call rather than once per import, so the
    /// whole call sees one consistent view.
    fn granted_for(&self, plugin_id: &str) -> HashSet<String>;

    /// Run a read-only statement, capped at `row_cap` rows.
    ///
    /// # Errors
    ///
    /// Whatever the driver reported, or [`ServiceError::Unsupported`].
    async fn db_query_read(
        &self,
        ctx: &CallCtx,
        sql: &str,
        row_cap: u32,
    ) -> Result<HostQueryResult, ServiceError> {
        let _ = (ctx, sql, row_cap);
        Err(ServiceError::Unsupported("db"))
    }

    /// Run a statement that may write.
    ///
    /// # Errors
    ///
    /// Whatever the driver reported, or [`ServiceError::Unsupported`].
    async fn db_execute(
        &self,
        ctx: &CallCtx,
        sql: &str,
    ) -> Result<HostStatementOutcome, ServiceError> {
        let _ = (ctx, sql);
        Err(ServiceError::Unsupported("db-write"))
    }

    /// Read the catalogue.
    ///
    /// # Errors
    ///
    /// Whatever the driver reported, or [`ServiceError::Unsupported`].
    async fn db_describe_schema(&self, ctx: &CallCtx) -> Result<HostSchema, ServiceError> {
        let _ = ctx;
        Err(ServiceError::Unsupported("db"))
    }

    /// Begin an explicit transaction.
    ///
    /// # Errors
    ///
    /// Whatever the driver reported, or [`ServiceError::Unsupported`].
    async fn db_tx_begin(&self, ctx: &CallCtx) -> Result<(), ServiceError> {
        let _ = ctx;
        Err(ServiceError::Unsupported("db-write"))
    }

    /// Commit the current transaction.
    ///
    /// # Errors
    ///
    /// Whatever the driver reported, or [`ServiceError::Unsupported`].
    async fn db_tx_commit(&self, ctx: &CallCtx) -> Result<(), ServiceError> {
        let _ = ctx;
        Err(ServiceError::Unsupported("db-write"))
    }

    /// Roll the current transaction back.
    ///
    /// # Errors
    ///
    /// Whatever the driver reported, or [`ServiceError::Unsupported`].
    async fn db_tx_rollback(&self, ctx: &CallCtx) -> Result<(), ServiceError> {
        let _ = ctx;
        Err(ServiceError::Unsupported("db-write"))
    }

    /// Invoke a host command by id.
    ///
    /// **Implementations must not re-enter [`crate::instance::InstanceRegistry`].**
    /// This runs while the calling plugin's store is locked, so routing
    /// a command back into a plugin would deadlock — and no epoch
    /// deadline would break it, because no wasm would be executing.
    /// [`crate::instance::PluginInstance::try_lock_store`] exists so a
    /// router can turn that into a busy error instead.
    ///
    /// # Errors
    ///
    /// Whatever the command reported, or [`ServiceError::Unsupported`].
    async fn invoke_command(
        &self,
        ctx: &CallCtx,
        command_id: &str,
        args_json: &str,
    ) -> Result<String, ServiceError> {
        let _ = (ctx, command_id, args_json);
        Err(ServiceError::Unsupported("command"))
    }

    /// Perform an outbound HTTP request that the host has already
    /// decided is permitted.
    ///
    /// # Errors
    ///
    /// Transport failure, or [`ServiceError::Unsupported`].
    async fn net_fetch(
        &self,
        ctx: &CallCtx,
        request: HostHttpRequest,
    ) -> Result<HostHttpResponse, ServiceError> {
        let _ = (ctx, request);
        Err(ServiceError::Unsupported("net"))
    }

    /// Read the system clipboard. `None` means it is empty.
    ///
    /// # Errors
    ///
    /// Backend failure, or [`ServiceError::Unsupported`].
    async fn clipboard_read(&self, ctx: &CallCtx) -> Result<Option<String>, ServiceError> {
        let _ = ctx;
        Err(ServiceError::Unsupported("clipboard"))
    }

    /// Write to the system clipboard.
    ///
    /// # Errors
    ///
    /// Backend failure, or [`ServiceError::Unsupported`].
    async fn clipboard_write(&self, ctx: &CallCtx, text: &str) -> Result<(), ServiceError> {
        let _ = (ctx, text);
        Err(ServiceError::Unsupported("clipboard"))
    }

    /// Post an OS notification.
    ///
    /// # Errors
    ///
    /// Backend failure, or [`ServiceError::Unsupported`].
    async fn notify_display(
        &self,
        ctx: &CallCtx,
        title: &str,
        body: &str,
    ) -> Result<(), ServiceError> {
        let _ = (ctx, title, body);
        Err(ServiceError::Unsupported("notify"))
    }

    /// Read a secret. The host has already namespaced `service`.
    ///
    /// # Errors
    ///
    /// Backend failure, or [`ServiceError::Unsupported`].
    async fn keyring_get(
        &self,
        ctx: &CallCtx,
        service: &str,
        account: &str,
    ) -> Result<Option<String>, ServiceError> {
        let _ = (ctx, service, account);
        Err(ServiceError::Unsupported("keyring"))
    }

    /// Write a secret. The host has already namespaced `service`.
    ///
    /// # Errors
    ///
    /// Backend failure, or [`ServiceError::Unsupported`].
    async fn keyring_set(
        &self,
        ctx: &CallCtx,
        service: &str,
        account: &str,
        secret: &str,
    ) -> Result<(), ServiceError> {
        let _ = (ctx, service, account, secret);
        Err(ServiceError::Unsupported("keyring"))
    }

    /// Delete a secret. The host has already namespaced `service`.
    ///
    /// # Errors
    ///
    /// Backend failure, or [`ServiceError::Unsupported`].
    async fn keyring_delete(
        &self,
        ctx: &CallCtx,
        service: &str,
        account: &str,
    ) -> Result<(), ServiceError> {
        let _ = (ctx, service, account);
        Err(ServiceError::Unsupported("keyring"))
    }
}

/// A shell that provides nothing.
///
/// The default on [`crate::host_impl::HostState`], so a host that has
/// not been handed services still constructs — and so every existing
/// caller kept compiling when the field was added. A plugin running
/// against this can log and receive events, which is exactly what the
/// host could do before any of this existed.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoServices;

impl HostServices for NoServices {
    fn granted_for(&self, _plugin_id: &str) -> HashSet<String> {
        HashSet::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ctx() -> CallCtx {
        CallCtx {
            plugin_id: "dev.plamenix.test".to_owned(),
            session_id: None,
        }
    }

    #[tokio::test]
    async fn the_default_shell_refuses_everything_by_naming_what_is_missing() {
        // A shell that implements nothing must produce errors an
        // operator can read, not empty successes that look like the
        // facility worked and returned nothing.
        let services = NoServices;
        assert_eq!(
            services.db_query_read(&ctx(), "SELECT 1", 100).await,
            Err(ServiceError::Unsupported("db")),
        );
        assert_eq!(
            services.keyring_get(&ctx(), "svc", "acct").await,
            Err(ServiceError::Unsupported("keyring")),
        );
        assert_eq!(
            services.clipboard_read(&ctx()).await,
            Err(ServiceError::Unsupported("clipboard")),
        );
    }

    #[test]
    fn a_shell_that_grants_nothing_is_still_a_valid_answer() {
        assert!(NoServices.granted_for("anything").is_empty());
    }

    #[tokio::test]
    async fn a_partial_shell_only_has_to_implement_what_it_has() {
        // The reason every method is defaulted: the web edition has no
        // keyring and should not write a stub saying so.
        struct DbOnly;

        #[async_trait::async_trait]
        impl HostServices for DbOnly {
            fn granted_for(&self, _: &str) -> HashSet<String> {
                HashSet::from(["db.read.any".to_owned()])
            }
            async fn db_query_read(
                &self,
                _: &CallCtx,
                _: &str,
                _: u32,
            ) -> Result<HostQueryResult, ServiceError> {
                Ok(HostQueryResult {
                    columns: vec![HostColumn {
                        name: "AMOUNT".to_owned(),
                        sql_type: None,
                        nullable: None,
                    }],
                    // The arm the mirror types exist to preserve.
                    rows: vec![HostRow {
                        cells: vec![HostCell::Decimal("12345.67".to_owned())],
                    }],
                    truncated: false,
                })
            }
        }

        let result = DbOnly.db_query_read(&ctx(), "SELECT 1", 10).await.unwrap();
        assert_eq!(
            result.rows[0].cells[0],
            HostCell::Decimal("12345.67".into())
        );
        assert_eq!(
            DbOnly.notify_display(&ctx(), "t", "b").await,
            Err(ServiceError::Unsupported("notify")),
        );
    }
}
