//! Row, column, and result types returned by [`crate::DbDriver`].

use serde::{Deserialize, Serialize};
use specta::Type;

/// Handle to a binary BLOB cached server-side. The full body is
/// fetched on demand via [`crate::DbDriver::fetch_blob`]; the row
/// payload only carries this small reference so large BLOBs do not
/// inflate every result row.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct BlobRef {
    /// Opaque identifier — `fetch_blob(session, id)` returns the body.
    pub id: String,
    /// Total BLOB length in bytes, surfaced so the UI can show size.
    pub size_bytes: i64,
    /// Lower-case hex preview of the first ~32 bytes. Enough to
    /// drive the table preview chip and the MIME sniffer.
    pub peek_hex: String,
}

/// Scalar values produced by Firebird columns.
///
/// The driver returns this typed enum rather than a single `String`
/// representation so the UI layer can render dates, numbers, and binary
/// blobs faithfully without re-parsing.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ColumnValue {
    /// SQL `NULL` for this column on this row.
    Null,
    /// Textual data (CHAR, VARCHAR, BLOB `SUB_TYPE` TEXT).
    Text(String),
    /// 64-bit signed integer (SMALLINT, INTEGER, BIGINT).
    Integer(i64),
    /// Double-precision floating point (FLOAT, DOUBLE PRECISION).
    Float(f64),
    /// Boolean (FB 3.0+ `BOOLEAN`).
    Bool(bool),
    /// Reference to a binary BLOB; full body fetched on demand.
    Blob(BlobRef),
}

/// A column description as reported by the driver.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct Column {
    /// Column name as reported by the Firebird engine (already
    /// upper-cased by Firebird unless quoted in the query).
    pub name: String,
}

/// A single result row.
#[derive(Clone, Debug, Default, Serialize, Deserialize, Type)]
pub struct Row {
    /// Cell values in column order.
    pub cells: Vec<ColumnValue>,
}

/// The shape returned by [`crate::DbDriver::execute`].
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub enum QueryResult {
    /// A `SELECT` (or similar) result: column metadata and rows.
    Rows {
        /// Column descriptions in declaration order.
        columns: Vec<Column>,
        /// Rows produced. May be truncated when `truncated` is `true`.
        rows: Vec<Row>,
        /// `true` when the caller injected a row cap (via
        /// [`inject_row_limit`]) and the server returned at least one
        /// row beyond the cap. The `rows` field is already trimmed to
        /// the cap; this flag tells the UI to surface a "truncated"
        /// hint and offer a re-run without the cap.
        #[serde(default)]
        truncated: bool,
    },
    /// A DML / DDL statement that returned no rows.
    Affected {
        /// Number of rows affected, when reported by the driver.
        rows: u64,
    },
}

/// Returns `true` when the statement appears to be a row-producing
/// query (SELECT / WITH / EXECUTE BLOCK). Uses the same heuristic as
/// the driver, kept here so the commands layer can decide whether to
/// inject a row cap.
#[must_use]
pub fn is_select_like(sql: &str) -> bool {
    sql.trim_start()
        .split_whitespace()
        .next()
        .is_some_and(|word| {
            word.eq_ignore_ascii_case("SELECT")
                || word.eq_ignore_ascii_case("WITH")
                || word.eq_ignore_ascii_case("EXECUTE")
        })
}

/// Appends a Firebird `ROWS 1 TO {max+1}` cap to a SELECT statement
/// so the driver pulls one extra row to detect truncation, then the
/// caller trims back to `max`. Statements that already contain a
/// row-limiting clause (`ROWS`, `FIRST`, `FETCH FIRST`) are returned
/// unchanged so the user-supplied cap wins.
///
/// Only the tail of the statement is inspected for the existing-cap
/// check; that's enough for the common case and avoids false positives
/// from column names or comments earlier in the query.
#[must_use]
pub fn inject_row_limit(sql: &str, max: u32) -> String {
    let trimmed = sql.trim_end().trim_end_matches(';').trim_end();
    let tail_start = trimmed.len().saturating_sub(120);
    let tail = trimmed[tail_start..].to_uppercase();
    if tail.contains(" ROWS ")
        || tail.contains(" FIRST ")
        || tail.contains(" FETCH FIRST")
        || tail.ends_with(" ROWS")
    {
        return trimmed.to_string();
    }
    format!("{trimmed}\nROWS 1 TO {}", max.saturating_add(1))
}

#[cfg(test)]
mod limit_tests {
    use super::{inject_row_limit, is_select_like};

    #[test]
    fn detects_select_and_with() {
        assert!(is_select_like("SELECT * FROM T"));
        assert!(is_select_like("  select 1 from rdb$database"));
        assert!(is_select_like("WITH cte AS (SELECT 1) SELECT * FROM cte"));
        assert!(is_select_like(
            "EXECUTE BLOCK RETURNS (n INT) AS BEGIN SUSPEND; END"
        ));
        assert!(!is_select_like("UPDATE t SET c = 1"));
        assert!(!is_select_like("DROP TABLE t"));
    }

    #[test]
    fn injects_cap_on_plain_select() {
        let out = inject_row_limit("SELECT * FROM T", 100);
        assert!(out.contains("ROWS 1 TO 101"));
    }

    #[test]
    fn leaves_existing_rows_clause_alone() {
        let out = inject_row_limit("SELECT * FROM T ROWS 1 TO 5", 100);
        assert!(!out.contains("ROWS 1 TO 101"));
    }

    #[test]
    fn leaves_first_clause_alone() {
        let out = inject_row_limit("SELECT FIRST 5 * FROM T", 100);
        assert!(!out.contains("ROWS"));
    }

    #[test]
    fn leaves_fetch_first_alone() {
        let out = inject_row_limit("SELECT * FROM T FETCH FIRST 5 ROWS ONLY", 100);
        assert!(!out.contains("ROWS 1 TO 101"));
    }

    #[test]
    fn trailing_semicolon_stripped() {
        let out = inject_row_limit("SELECT 1 FROM rdb$database;", 50);
        assert!(out.ends_with("ROWS 1 TO 51"));
    }
}

/// Outcome of one statement in a multi-statement batch.
///
/// The Tauri / web command handler splits the incoming SQL into
/// individual statements with [`split_statements`], executes each, and
/// returns one of these per statement. Execution stops at the first
/// failure so the user sees exactly which statement broke the batch.
#[derive(Clone, Debug, Serialize, Deserialize, Type)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum StatementOutcome {
    /// Statement ran successfully.
    Ok {
        /// The exact SQL text that was executed (with surrounding
        /// whitespace trimmed, semicolon stripped).
        sql: String,
        /// Driver-level result.
        result: QueryResult,
        /// Wall-clock duration in milliseconds.
        duration_ms: u64,
    },
    /// Statement failed; the batch aborted after this entry.
    Err {
        /// The SQL text that was attempted.
        sql: String,
        /// Stringified driver error.
        error: String,
        /// Wall-clock duration in milliseconds (typically near-zero for
        /// validation failures).
        duration_ms: u64,
    },
}

/// Splits a SQL batch into individual statements at top-level `;`.
///
/// Honours Firebird string and identifier quoting (`'foo''bar'` and
/// `"id ""esc"""`), line comments (`-- to end of line`), and block
/// comments (`/* ... */`). Comment-only or whitespace-only statements
/// are dropped — they would only confuse the driver.
///
/// The result is allocation-light: trimmed `String` per statement.
///
/// # Examples
///
/// ```
/// let stmts = plamenix_db::split_statements(
///     "SELECT 1; -- comment\nUPDATE t SET c = ';' WHERE id = 1;"
/// );
/// assert_eq!(stmts.len(), 2);
/// assert_eq!(stmts[0], "SELECT 1");
/// assert!(stmts[1].contains("UPDATE"));
/// ```
#[must_use]
pub fn split_statements(sql: &str) -> Vec<String> {
    enum State {
        Normal,
        SingleQuote,
        DoubleQuote,
        LineComment,
        BlockComment,
    }

    let bytes = sql.as_bytes();
    let mut state = State::Normal;
    let mut start = 0usize;
    let mut has_content = false;
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match state {
            State::Normal => match c {
                b'\'' => {
                    state = State::SingleQuote;
                    has_content = true;
                }
                b'"' => {
                    state = State::DoubleQuote;
                    has_content = true;
                }
                b'-' if bytes.get(i + 1).copied() == Some(b'-') => {
                    state = State::LineComment;
                    i += 1;
                }
                b'/' if bytes.get(i + 1).copied() == Some(b'*') => {
                    state = State::BlockComment;
                    i += 1;
                }
                b';' => {
                    if has_content {
                        out.push(sql[start..i].trim().to_string());
                    }
                    start = i + 1;
                    has_content = false;
                }
                _ if !c.is_ascii_whitespace() => {
                    has_content = true;
                }
                _ => {}
            },
            State::SingleQuote => {
                if c == b'\'' {
                    if bytes.get(i + 1).copied() == Some(b'\'') {
                        i += 1;
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::DoubleQuote => {
                if c == b'"' {
                    if bytes.get(i + 1).copied() == Some(b'"') {
                        i += 1;
                    } else {
                        state = State::Normal;
                    }
                }
            }
            State::LineComment => {
                if c == b'\n' {
                    state = State::Normal;
                }
            }
            State::BlockComment => {
                if c == b'*' && bytes.get(i + 1).copied() == Some(b'/') {
                    state = State::Normal;
                    i += 1;
                }
            }
        }
        i += 1;
    }
    if has_content {
        out.push(sql[start..].trim().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::split_statements;

    #[test]
    fn empty_input_yields_nothing() {
        assert!(split_statements("").is_empty());
        assert!(split_statements("   \n\t  ").is_empty());
        assert!(split_statements(";;;;").is_empty());
    }

    #[test]
    fn single_statement_without_semicolon() {
        let stmts = split_statements("SELECT 1");
        assert_eq!(stmts, vec!["SELECT 1".to_string()]);
    }

    #[test]
    fn two_statements() {
        let stmts = split_statements("SELECT 1; UPDATE t SET c = 2");
        assert_eq!(
            stmts,
            vec!["SELECT 1".to_string(), "UPDATE t SET c = 2".to_string()]
        );
    }

    #[test]
    fn semicolon_inside_string_is_not_a_separator() {
        let stmts = split_statements("UPDATE t SET c = ';' WHERE id = 1; SELECT 1");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("';'"));
        assert_eq!(stmts[1], "SELECT 1");
    }

    #[test]
    fn escaped_quotes_in_strings() {
        let stmts = split_statements("SELECT 'O''Brien;' AS name; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("'O''Brien;'"));
    }

    #[test]
    fn quoted_identifiers() {
        let stmts = split_statements(r#"SELECT * FROM "weird;name"; SELECT 2"#);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains(r#""weird;name""#));
    }

    #[test]
    fn line_comments_swallow_semicolons() {
        let stmts = split_statements("SELECT 1; -- end ;\nSELECT 2");
        assert_eq!(
            stmts,
            vec!["SELECT 1".to_string(), "-- end ;\nSELECT 2".to_string()]
        );
    }

    #[test]
    fn block_comments_swallow_semicolons() {
        let stmts = split_statements("SELECT 1 /* ; nope */; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn comment_only_statements_dropped() {
        let stmts = split_statements("-- just a comment;\n/* also a comment */;");
        assert!(stmts.is_empty());
    }

    #[test]
    fn trailing_semicolon_omitted_from_statement() {
        let stmts = split_statements("SELECT 1;");
        assert_eq!(stmts, vec!["SELECT 1".to_string()]);
    }
}
