//! Plugin capability grants, in Firebird.
//!
//! The record of what a user approved for which plugin. It is the
//! authority the plugin host's `granted_for` reads, so a grant that
//! fails to persist is a capability the user approved once and has to
//! approve again on every restart.

use std::collections::HashMap;

use plamenix_db::{ColumnValue, DbDriver, QueryResult};

use crate::store::{MetaError, MetaStore, quote};

impl MetaStore {
    /// Records a grant. Re-granting is a no-op rather than an error.
    ///
    /// The pair is the primary key, so the insert is guarded by an
    /// existence check — Firebird has no `INSERT OR IGNORE`, and
    /// swallowing a primary-key violation would also swallow the ones
    /// that mean something else.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the write fails.
    pub async fn grant(&self, plugin_id: &str, permission: &str) -> Result<(), MetaError> {
        let sql = format!(
            "UPDATE OR INSERT INTO PLUGIN_GRANTS (PLUGIN_ID, PERMISSION, GRANTED_AT) \
             VALUES ({}, {}, {}) MATCHING (PLUGIN_ID, PERMISSION)",
            quote(Some(plugin_id)),
            quote(Some(permission)),
            crate::store::now_millis(),
        );
        self.exec(sql).await
    }

    /// Withdraws a grant. Revoking one that was never granted is a
    /// no-op — the caller's intent is satisfied either way.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the delete fails.
    pub async fn revoke(&self, plugin_id: &str, permission: &str) -> Result<(), MetaError> {
        self.exec(format!(
            "DELETE FROM PLUGIN_GRANTS WHERE PLUGIN_ID = {} AND PERMISSION = {}",
            quote(Some(plugin_id)),
            quote(Some(permission)),
        ))
        .await
    }

    /// Capabilities granted to one plugin.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the query fails.
    pub async fn granted_for(&self, plugin_id: &str) -> Result<Vec<String>, MetaError> {
        let sql = format!(
            "SELECT PERMISSION FROM PLUGIN_GRANTS WHERE PLUGIN_ID = {} ORDER BY PERMISSION",
            quote(Some(plugin_id)),
        );
        let result = self
            .driver()
            .execute(self.session(), sql)
            .await
            .map_err(|err| MetaError::Driver(err.to_string()))?;
        let QueryResult::Rows { rows, .. } = result else {
            return Ok(Vec::new());
        };
        Ok(rows
            .into_iter()
            .filter_map(|row| match row.cells.first() {
                Some(ColumnValue::Text(text)) => Some(text.clone()),
                _ => None,
            })
            .collect())
    }

    /// Every grant, by plugin. Read once at boot to replay into the
    /// plugin host.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the query fails.
    pub async fn all_grants(&self) -> Result<HashMap<String, Vec<String>>, MetaError> {
        let result = self
            .driver()
            .execute(
                self.session(),
                "SELECT PLUGIN_ID, PERMISSION FROM PLUGIN_GRANTS ORDER BY PLUGIN_ID, PERMISSION"
                    .to_owned(),
            )
            .await
            .map_err(|err| MetaError::Driver(err.to_string()))?;

        let QueryResult::Rows { rows, .. } = result else {
            return Ok(HashMap::new());
        };
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let (Some(ColumnValue::Text(plugin)), Some(ColumnValue::Text(permission))) =
                (row.cells.first(), row.cells.get(1))
            else {
                continue;
            };
            out.entry(plugin.clone())
                .or_default()
                .push(permission.clone());
        }
        Ok(out)
    }

    /// Drops every grant for one plugin, for uninstall.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the delete fails.
    pub async fn purge_plugin(&self, plugin_id: &str) -> Result<(), MetaError> {
        self.exec(format!(
            "DELETE FROM PLUGIN_GRANTS WHERE PLUGIN_ID = {}",
            quote(Some(plugin_id))
        ))
        .await
    }
}
