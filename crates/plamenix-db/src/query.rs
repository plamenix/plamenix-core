//! Row, column, and result types returned by [`crate::DbDriver`].

use serde::{Deserialize, Serialize};

/// Scalar values produced by Firebird columns.
///
/// The driver returns this typed enum rather than a single `String`
/// representation so the UI layer can render dates, numbers, and binary
/// blobs faithfully without re-parsing.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    /// Binary blob, base64-encoded for transport.
    Blob(String),
}

/// A column description as reported by the driver.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    /// Column name as reported by the Firebird engine (already
    /// upper-cased by Firebird unless quoted in the query).
    pub name: String,
}

/// A single result row.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Row {
    /// Cell values in column order.
    pub cells: Vec<ColumnValue>,
}

/// The shape returned by [`crate::DbDriver::execute`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum QueryResult {
    /// A `SELECT` (or similar) result: column metadata and rows.
    Rows {
        /// Column descriptions in declaration order.
        columns: Vec<Column>,
        /// All rows produced. Pagination is the caller's concern; the
        /// driver returns whatever the statement produced.
        rows: Vec<Row>,
    },
    /// A DML / DDL statement that returned no rows.
    Affected {
        /// Number of rows affected, when reported by the driver.
        rows: u64,
    },
}
