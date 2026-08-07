//! Opening the metadata database, and writing to the audit log.

use std::path::{Path, PathBuf};

use plamenix_db::{ConnectMode, DbDriver, RsfbDriver};
use plamenix_types::{ConnectionConfig, SessionId};
use serde::{Deserialize, Serialize};

/// Why the metadata store could not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    /// Another process is holding the database open.
    ///
    /// Its own variant because it is the one failure a user can act on:
    /// Firebird's embedded engine takes the file exclusively, so a
    /// second instance — or a crashed one that has not released yet —
    /// produces this and nothing else will fix it.
    #[error(
        "the metadata database at {path} is held by another process; close any other Plamenix instance and retry"
    )]
    Locked { path: PathBuf },
    /// The driver refused.
    #[error("metadata store: {0}")]
    Driver(String),
    /// The database file could not be created or reached.
    #[error("metadata store at {path}: {detail}")]
    Io { path: PathBuf, detail: String },
}

/// One line in the audit log.
///
/// Deliberately not a free-form string. A log nobody can query is a log
/// nobody reads, and the fields below are the ones a question is
/// actually asked about: who, what, to what, and did it work.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEntry {
    /// Token name that authenticated the request, when one did.
    /// `None` for events refused before authentication — a rejected
    /// Host, a missing token — which are precisely the ones worth
    /// recording.
    pub actor: Option<String>,
    /// Where the request came from.
    pub remote_addr: Option<String>,
    /// What was attempted, as a stable identifier rather than prose.
    pub action: String,
    /// What it was attempted against, when that is meaningful.
    pub target: Option<String>,
    /// `allowed` or `refused`.
    pub outcome: String,
    /// Anything else worth keeping, as JSON.
    pub detail: Option<String>,
}

/// Plamenix's own metadata database.
///
/// Holds one embedded Firebird session for the process lifetime. The
/// engine takes the file exclusively, so this is deliberately not
/// something to open twice.
pub struct MetaStore {
    driver: RsfbDriver,
    session: SessionId,
    path: PathBuf,
}

impl std::fmt::Debug for MetaStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetaStore")
            .field("path", &self.path)
            .finish()
    }
}

impl MetaStore {
    /// Opens — creating if absent — the metadata database at `path`.
    ///
    /// `fbclient_path` must point at a full Firebird install rather than
    /// a bare client library: embedded mode needs the engine plugin,
    /// `firebird.conf`, and `firebird.msg` alongside it. Both editions
    /// ship exactly that under `resources/fbclient/`.
    ///
    /// # Errors
    ///
    /// [`MetaError::Locked`] when another process holds the file,
    /// [`MetaError::Io`] when the directory cannot be created, and
    /// [`MetaError::Driver`] for anything the engine refused.
    pub async fn open(
        path: impl AsRef<Path>,
        fbclient_path: Option<String>,
    ) -> Result<Self, MetaError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| MetaError::Io {
                path: path.clone(),
                detail: err.to_string(),
            })?;
        }

        let config = ConnectionConfig {
            host: String::new(),
            port: 0,
            database: path.to_string_lossy().into_owned(),
            user: "SYSDBA".to_owned(),
            password: "masterkey".to_owned(),
            charset: Some("UTF8".to_owned()),
            encryption_key: None,
            fbclient_path,
            encryption_required: false,
            embedded: true,
        };

        // Attaching and creating are different operations in Firebird,
        // and `connect` only attaches. First run has no file to attach
        // to, so create it — guarded on existence, because "create" on
        // an existing database is an error and not a benign one.
        if !path.exists() {
            plamenix_db::rsfb::create_embedded_database(&config).map_err(|err| MetaError::Io {
                path: path.clone(),
                detail: format!("could not create the metadata database: {err}"),
            })?;
        }

        let driver = RsfbDriver::new();
        let session = driver
            .connect(config, ConnectMode::Native)
            .await
            .map_err(|err| {
                let text = err.to_string();
                // The engine reports an exclusive-access conflict in
                // several wordings depending on platform and version;
                // matching on the recognisable parts is better than
                // reporting a lock as a generic driver failure the user
                // cannot act on.
                if text.contains("lock") || text.contains("in use") || text.contains("shutdown") {
                    MetaError::Locked { path: path.clone() }
                } else {
                    MetaError::Driver(text)
                }
            })?;

        let store = Self {
            driver,
            session,
            path,
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Brings the schema up to date. Safe to run on every open.
    async fn migrate(&self) -> Result<(), MetaError> {
        for statement in crate::schema::SCHEMA_STATEMENTS {
            self.driver
                .execute(self.session, (*statement).to_owned())
                .await
                .map_err(|err| MetaError::Driver(err.to_string()))?;
        }
        Ok(())
    }

    /// Appends one entry to the audit log.
    ///
    /// Returns `Result` rather than swallowing, but callers on a
    /// request path should log a failure and carry on: an audit write
    /// that fails is worth knowing about and is not worth failing the
    /// user's operation over. The alternative — refusing to serve when
    /// the log is unwritable — is a denial of service triggered by a
    /// full disk.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the insert fails.
    pub async fn record(&self, entry: &AuditEntry) -> Result<(), MetaError> {
        let sql = format!(
            "INSERT INTO AUDIT_LOG (OCCURRED_AT, ACTOR, REMOTE_ADDR, ACTION, TARGET, OUTCOME, DETAIL) \
             VALUES ({}, {}, {}, {}, {}, {}, {})",
            now_millis(),
            quote(entry.actor.as_deref()),
            quote(entry.remote_addr.as_deref()),
            quote(Some(&entry.action)),
            quote(entry.target.as_deref()),
            quote(Some(&entry.outcome)),
            quote(entry.detail.as_deref()),
        );
        self.driver
            .execute(self.session, sql)
            .await
            .map(|_| ())
            .map_err(|err| MetaError::Driver(err.to_string()))
    }

    /// The session other stores share, so history and grants do not each
    /// open their own exclusive attachment.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// The driver behind this store.
    #[must_use]
    pub const fn driver(&self) -> &RsfbDriver {
        &self.driver
    }

    /// Runs a statement that returns nothing.
    pub(crate) async fn exec(&self, sql: String) -> Result<(), MetaError> {
        self.driver
            .execute(self.session, sql)
            .await
            .map(|_| ())
            .map_err(|err| MetaError::Driver(err.to_string()))
    }

    /// Runs a statement and reports how many rows it changed.
    pub(crate) async fn affected(&self, sql: String) -> Result<u64, MetaError> {
        let result = self
            .driver
            .execute(self.session, sql)
            .await
            .map_err(|err| MetaError::Driver(err.to_string()))?;
        Ok(match result {
            plamenix_db::QueryResult::Affected { rows } => rows,
            plamenix_db::QueryResult::Rows { rows, .. } => rows.len() as u64,
        })
    }

    /// Where the database lives.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Renders a value as a SQL literal, or `NULL`.
///
/// Every string reaching this is host-produced — an action name, a
/// token name, an address — but escaping is done anyway. An audit log
/// that can be corrupted by the thing it is auditing is not an audit
/// log, and a token name is one config edit away from containing a
/// quote.
pub(crate) fn quote(value: Option<&str>) -> String {
    match value {
        None => "NULL".to_owned(),
        Some(text) => format!("'{}'", text.replace('\'', "''")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_quote_in_a_value_cannot_break_out_of_its_literal() {
        assert_eq!(quote(Some("o'brien")), "'o''brien'");
        assert_eq!(
            quote(Some("'; DROP TABLE AUDIT_LOG --")),
            "'''; DROP TABLE AUDIT_LOG --'"
        );
        assert_eq!(quote(None), "NULL");
    }
}
