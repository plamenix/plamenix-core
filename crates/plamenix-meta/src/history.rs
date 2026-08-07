//! Query history, in Firebird.
//!
//! One implementation. This table previously existed twice with the
//! same schema — `rusqlite` in the desktop shell and `better-sqlite3`
//! in the web server — so a change to it meant two edits in two
//! languages, and drift between them was a matter of time rather than
//! chance.

use plamenix_db::{ColumnValue, DbDriver, QueryResult};
use serde::{Deserialize, Serialize};

use crate::store::{MetaError, MetaStore, quote};

/// One recorded statement.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub id: i64,
    pub profile_id: String,
    pub sql: String,
    pub executed_at: i64,
    pub duration_ms: i64,
    pub status: String,
    pub error: Option<String>,
    pub rows_returned: Option<i64>,
    pub label: Option<String>,
}

/// What to record about a statement that just ran.
#[derive(Clone, Debug)]
pub struct NewHistoryEntry<'a> {
    pub profile_id: &'a str,
    pub sql: &'a str,
    pub executed_at: i64,
    pub duration_ms: i64,
    pub status: &'a str,
    pub error: Option<&'a str>,
    pub rows_returned: Option<i64>,
    pub label: Option<&'a str>,
}

impl MetaStore {
    /// Records one statement.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the insert fails.
    pub async fn record_history(&self, entry: &NewHistoryEntry<'_>) -> Result<(), MetaError> {
        let sql = format!(
            "INSERT INTO QUERY_HISTORY \
             (PROFILE_ID, SQL_TEXT, EXECUTED_AT, DURATION_MS, STATUS, ERROR, ROWS_RETURNED, LABEL) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {})",
            quote(Some(entry.profile_id)),
            quote(Some(entry.sql)),
            entry.executed_at,
            entry.duration_ms,
            quote(Some(entry.status)),
            quote(entry.error),
            entry
                .rows_returned
                .map_or("NULL".to_owned(), |n| n.to_string()),
            quote(entry.label),
        );
        self.exec(sql).await
    }

    /// Most recent statements for one profile, newest first.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the query fails.
    pub async fn list_history(
        &self,
        profile_id: &str,
        limit: i64,
    ) -> Result<Vec<HistoryEntry>, MetaError> {
        // `FIRST n` is Firebird's row cap; the clamp keeps a caller
        // from asking for the whole table by passing a negative or
        // absurd value, which SQLite's LIMIT would have tolerated.
        let capped = limit.clamp(1, 10_000);
        let sql = format!(
            "SELECT FIRST {capped} ID, PROFILE_ID, SQL_TEXT, EXECUTED_AT, DURATION_MS, STATUS, \
             ERROR, ROWS_RETURNED, LABEL FROM QUERY_HISTORY WHERE PROFILE_ID = {} \
             ORDER BY EXECUTED_AT DESC",
            quote(Some(profile_id)),
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
            .filter_map(|row| to_entry(&row.cells))
            .collect())
    }

    /// Removes every entry for one profile, returning how many went.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the delete fails.
    pub async fn clear_history(&self, profile_id: &str) -> Result<u64, MetaError> {
        self.affected(format!(
            "DELETE FROM QUERY_HISTORY WHERE PROFILE_ID = {}",
            quote(Some(profile_id))
        ))
        .await
    }

    /// Sets or clears one entry's label.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the update fails.
    pub async fn set_history_label(&self, id: i64, label: Option<&str>) -> Result<bool, MetaError> {
        let affected = self
            .affected(format!(
                "UPDATE QUERY_HISTORY SET LABEL = {} WHERE ID = {id}",
                quote(label)
            ))
            .await?;
        Ok(affected > 0)
    }

    /// Deletes one entry.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the delete fails.
    pub async fn delete_history(&self, id: i64) -> Result<bool, MetaError> {
        let affected = self
            .affected(format!("DELETE FROM QUERY_HISTORY WHERE ID = {id}"))
            .await?;
        Ok(affected > 0)
    }

    /// Deletes several entries.
    ///
    /// # Errors
    ///
    /// [`MetaError::Driver`] when the delete fails.
    pub async fn delete_history_many(&self, ids: &[i64]) -> Result<u64, MetaError> {
        if ids.is_empty() {
            // An empty `IN ()` is a syntax error, and "delete nothing"
            // is a perfectly reasonable thing for a caller to ask.
            return Ok(0);
        }
        let list = ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        self.affected(format!("DELETE FROM QUERY_HISTORY WHERE ID IN ({list})"))
            .await
    }
}

/// Builds an entry from a row, or `None` when the shape is unexpected.
///
/// Skipping a malformed row rather than failing the whole list: history
/// is a convenience, and one unreadable row should not cost the user
/// the other nine hundred.
fn to_entry(cells: &[ColumnValue]) -> Option<HistoryEntry> {
    Some(HistoryEntry {
        id: as_int(cells.first()?)?,
        profile_id: as_text(cells.get(1)?)?,
        sql: as_text(cells.get(2)?)?,
        executed_at: as_int(cells.get(3)?)?,
        duration_ms: as_int(cells.get(4)?)?,
        status: as_text(cells.get(5)?)?,
        error: cells.get(6).and_then(as_text),
        rows_returned: cells.get(7).and_then(as_int),
        label: cells.get(8).and_then(as_text),
    })
}

fn as_int(cell: &ColumnValue) -> Option<i64> {
    match cell {
        ColumnValue::Integer(value) => Some(*value),
        // A NUMERIC column comes back as exact text; history counts are
        // whole numbers, so parsing is safe here in a way it would not
        // be for user data.
        ColumnValue::Decimal(text) => text.parse().ok(),
        _ => None,
    }
}

fn as_text(cell: &ColumnValue) -> Option<String> {
    match cell {
        ColumnValue::Text(text) => Some(text.clone()),
        ColumnValue::Null => None,
        other => Some(format!("{other:?}")),
    }
}
