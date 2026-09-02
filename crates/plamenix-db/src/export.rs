//! Server-side export formatters.
//!
//! Builds CSV / JSON / SQL / XML deliverables from `Column` + `Row`
//! payloads. The TS-side equivalents live in
//! `plamenix-ui/src/db/cell-format.ts`; these functions preserve the
//! same wire-shape so server- and client-built output stays
//! interchangeable when migrating consumers.
//!
//! XLSX is intentionally not handled here — the `write-excel-file`
//! browser library produces the binary blob from typed JS cells, and
//! we keep that step client-side.

use plamenix_types::{ColumnInfo, TableInfo};

use crate::query::{BlobRef, Column, ColumnValue, Row};

/// CSV field separator picked by the user in the export modal.
#[derive(Clone, Copy, Debug)]
pub enum CsvDelimiter {
    /// `,` — the standard CSV separator.
    Comma,
    /// `;` — common in EU-locale spreadsheet imports.
    Semicolon,
    /// `\t` — tab-separated values (TSV).
    Tab,
}

impl CsvDelimiter {
    fn as_char(self) -> char {
        match self {
            CsvDelimiter::Comma => ',',
            CsvDelimiter::Semicolon => ';',
            CsvDelimiter::Tab => '\t',
        }
    }
}

fn blob_summary(b: &BlobRef) -> String {
    format!("BLOB({} bytes, peek=0x{})", b.size_bytes, b.peek_hex)
}

fn needs_csv_quote(s: &str, delim: char) -> bool {
    s.chars()
        .any(|c| c == '"' || c == '\r' || c == '\n' || c == delim)
}

fn quote_csv_field(s: &str, delim: char) -> String {
    if needs_csv_quote(s, delim) {
        let escaped = s.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

fn cell_to_csv(cell: &ColumnValue, delim: char) -> String {
    match cell {
        ColumnValue::Null => String::new(),
        ColumnValue::Text(v) => quote_csv_field(v, delim),
        ColumnValue::Integer(v) => v.to_string(),
        // Already exact decimal text; emit it verbatim like any number.
        ColumnValue::Decimal(v) => v.clone(),
        ColumnValue::Float(v) => v.to_string(),
        ColumnValue::Bool(true) => "true".to_string(),
        ColumnValue::Bool(false) => "false".to_string(),
        ColumnValue::Blob(b) => quote_csv_field(&blob_summary(b), delim),
    }
}

fn cell_to_sql_literal(cell: &ColumnValue) -> String {
    match cell {
        ColumnValue::Null => "NULL".to_string(),
        ColumnValue::Text(v) => format!("'{}'", v.replace('\'', "''")),
        ColumnValue::Integer(v) => v.to_string(),
        // Bare numeric literal: quoting would make Firebird re-parse it
        // as a string on import.
        ColumnValue::Decimal(v) => v.clone(),
        ColumnValue::Float(v) => v.to_string(),
        ColumnValue::Bool(true) => "TRUE".to_string(),
        ColumnValue::Bool(false) => "FALSE".to_string(),
        ColumnValue::Blob(b) => format!("/* {} */ NULL", blob_summary(b)),
    }
}

fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn cell_to_xml_text(cell: &ColumnValue) -> String {
    match cell {
        ColumnValue::Null => String::new(),
        ColumnValue::Text(v) => v.clone(),
        ColumnValue::Integer(v) => v.to_string(),
        ColumnValue::Decimal(v) => v.clone(),
        ColumnValue::Float(v) => v.to_string(),
        ColumnValue::Bool(true) => "true".to_string(),
        ColumnValue::Bool(false) => "false".to_string(),
        ColumnValue::Blob(b) => blob_summary(b),
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn cell_to_json(cell: &ColumnValue) -> serde_json::Value {
    match cell {
        ColumnValue::Null => serde_json::Value::Null,
        ColumnValue::Text(v) => serde_json::Value::String(v.clone()),
        ColumnValue::Integer(v) => serde_json::Value::from(*v),
        // Text, not a JSON number: the exactness would be lost the
        // moment any JavaScript consumer called JSON.parse.
        ColumnValue::Decimal(v) => serde_json::Value::String(v.clone()),
        ColumnValue::Float(v) => serde_json::Number::from_f64(*v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        ColumnValue::Bool(v) => serde_json::Value::Bool(*v),
        ColumnValue::Blob(b) => serde_json::Value::String(blob_summary(b)),
    }
}

/// A single statement's contribution to an export. For database-wide
/// exports the host pushes one entry per table; for result-table
/// exports the host pushes a single entry.
#[derive(Debug)]
pub struct ExportPart<'a> {
    /// Schema metadata for the source table (drives `CREATE TABLE` in
    /// SQL exports). `None` when the source is not a known base
    /// table — DDL is then omitted.
    pub table: Option<&'a TableInfo>,
    /// User-visible label for the part (typically the table name).
    /// Drives per-part markers in CSV/SQL and keys in JSON/XML.
    pub label: Option<String>,
    /// Result-column descriptions.
    pub columns: &'a [Column],
    /// Rows produced by the source statement. Ownership is borrowed
    /// because the caller usually already holds the rows.
    pub rows: &'a [Row],
}

/// Builds the entire CSV body from one or more parts. Per-part markers
/// (`-- TABLE <name>`) are emitted when `label` is present.
pub fn format_csv(parts: &[ExportPart<'_>], delim: CsvDelimiter) -> String {
    let mut out = String::new();
    let d = delim.as_char();
    let push_row = |out: &mut String, cells: Vec<String>| {
        let mut first = true;
        for cell in cells {
            if !first {
                out.push(d);
            }
            out.push_str(&cell);
            first = false;
        }
        out.push_str("\r\n");
    };
    for (idx, part) in parts.iter().enumerate() {
        if let Some(label) = part.label.as_deref() {
            if idx > 0 {
                out.push_str("\r\n");
            }
            out.push_str(&format!("-- TABLE {label}\r\n"));
        }
        push_row(
            &mut out,
            part.columns
                .iter()
                .map(|c| quote_csv_field(&c.name, d))
                .collect(),
        );
        for row in part.rows {
            push_row(
                &mut out,
                row.cells.iter().map(|c| cell_to_csv(c, d)).collect(),
            );
        }
    }
    out
}

/// JSON output: when `parts.len() == 1` returns an array of row
/// objects; for multi-part exports returns an object keyed by part
/// label (or 0-indexed string when `label` is missing).
pub fn format_json(parts: &[ExportPart<'_>]) -> String {
    if parts.len() == 1 {
        let p = &parts[0];
        let rows: Vec<serde_json::Value> =
            p.rows.iter().map(|r| row_to_object(p.columns, r)).collect();
        return serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string());
    }
    let mut map = serde_json::Map::new();
    for (idx, part) in parts.iter().enumerate() {
        let key = part.label.clone().unwrap_or_else(|| idx.to_string());
        let rows: Vec<serde_json::Value> = part
            .rows
            .iter()
            .map(|r| row_to_object(part.columns, r))
            .collect();
        map.insert(key, serde_json::Value::Array(rows));
    }
    serde_json::to_string_pretty(&serde_json::Value::Object(map))
        .unwrap_or_else(|_| "{}".to_string())
}

fn row_to_object(columns: &[Column], row: &Row) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    for (i, col) in columns.iter().enumerate() {
        let value = row
            .cells
            .get(i)
            .map(cell_to_json)
            .unwrap_or(serde_json::Value::Null);
        obj.insert(col.name.clone(), value);
    }
    serde_json::Value::Object(obj)
}

/// SQL output: optional `CREATE TABLE` DDL (driven by `table` carrying
/// `ColumnInfo` + primary-key segments) followed by one
/// `INSERT INTO … VALUES (…)` per row. For multi-table exports each
/// part is delimited with a banner comment.
///
/// When `include_ddl` is false the `CREATE TABLE` block is skipped and
/// the output is `INSERT`-only. The banner comment is still emitted
/// so multi-part dumps remain visually segmented.
pub fn format_sql(parts: &[ExportPart<'_>], include_ddl: bool) -> String {
    let mut out = String::new();
    if parts.len() != 1 {
        out.push_str("-- Plamenix database export\n");
        out.push_str(&format!("-- {}\n\n", chrono_like_now()));
    }
    for part in parts {
        if let Some(label) = part.label.as_deref() {
            out.push_str("-- ============================================\n");
            out.push_str(&format!("-- TABLE {label}\n"));
            out.push_str("-- ============================================\n");
        }
        if include_ddl {
            if let Some(table) = part.table {
                out.push_str(&build_create_table(table));
                out.push_str("\n\n");
            }
        }
        if part.rows.is_empty() {
            continue;
        }
        let ident = part
            .label
            .as_deref()
            .map(quote_ident)
            .unwrap_or_else(|| "<TABLE>".to_string());
        let col_list: Vec<String> = part.columns.iter().map(|c| quote_ident(&c.name)).collect();
        let col_list_str = col_list.join(", ");
        for row in part.rows {
            let values: Vec<String> = row.cells.iter().map(cell_to_sql_literal).collect();
            out.push_str(&format!(
                "INSERT INTO {ident} ({col_list_str}) VALUES ({});\n",
                values.join(", ")
            ));
        }
        out.push('\n');
    }
    out
}

fn build_create_table(table: &TableInfo) -> String {
    let mut lines: Vec<String> = table
        .columns
        .iter()
        .map(|c: &ColumnInfo| {
            let null = if c.nullable { "" } else { " NOT NULL" };
            let def = c
                .default_expr
                .as_deref()
                .map(|d| format!(" DEFAULT {d}"))
                .unwrap_or_default();
            format!("    {} {}{}{}", quote_ident(&c.name), c.sql_type, def, null)
        })
        .collect();
    if !table.primary_key.is_empty() {
        let pk: Vec<String> = table.primary_key.iter().map(|s| quote_ident(s)).collect();
        lines.push(format!("    PRIMARY KEY ({})", pk.join(", ")));
    }
    format!(
        "CREATE TABLE {} (\n{}\n);",
        quote_ident(&table.name),
        lines.join(",\n")
    )
}

/// XML output: nested `<rows>` (single statement) or
/// `<database><table>…<row>…<col>` (multi-part).
pub fn format_xml(parts: &[ExportPart<'_>]) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    if parts.len() == 1 && parts[0].label.is_none() {
        push_xml_rows(&mut out, parts[0].columns, parts[0].rows, "rows", "");
    } else {
        out.push_str("<database>\n");
        for part in parts {
            let label = part.label.as_deref().unwrap_or("");
            out.push_str(&format!("  <table name=\"{}\">\n", escape_xml(label)));
            push_xml_rows(&mut out, part.columns, part.rows, "row", "    ");
            out.push_str("  </table>\n");
        }
        out.push_str("</database>\n");
    }
    out
}

fn push_xml_rows(out: &mut String, columns: &[Column], rows: &[Row], outer: &str, indent: &str) {
    if outer == "rows" {
        out.push_str("<rows>\n");
        for row in rows {
            push_one_xml_row(out, columns, row, "  ");
        }
        out.push_str("</rows>\n");
    } else {
        for row in rows {
            out.push_str(indent);
            out.push_str("<row>\n");
            let inner_indent = format!("{indent}  ");
            push_xml_cols(out, columns, row, &inner_indent);
            out.push_str(indent);
            out.push_str("</row>\n");
        }
    }
}

fn push_one_xml_row(out: &mut String, columns: &[Column], row: &Row, indent: &str) {
    out.push_str(indent);
    out.push_str("<row>\n");
    let inner_indent = format!("{indent}  ");
    push_xml_cols(out, columns, row, &inner_indent);
    out.push_str(indent);
    out.push_str("</row>\n");
}

fn push_xml_cols(out: &mut String, columns: &[Column], row: &Row, indent: &str) {
    for (i, col) in columns.iter().enumerate() {
        let cell = row.cells.get(i);
        let is_null = matches!(cell, None | Some(ColumnValue::Null));
        let null_attr = if is_null { " null=\"true\"" } else { "" };
        let text = match cell {
            Some(c) if !matches!(c, ColumnValue::Null) => escape_xml(&cell_to_xml_text(c)),
            _ => String::new(),
        };
        out.push_str(&format!(
            "{indent}<col name=\"{}\"{null_attr}>{text}</col>\n",
            escape_xml(&col.name)
        ));
    }
}

fn chrono_like_now() -> String {
    // Avoid pulling chrono just for an export banner. ISO-ish timestamp
    // built from std `SystemTime`.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs} (unix seconds)")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> Column {
        Column {
            name: name.to_string(),
        }
    }

    #[test]
    fn csv_quotes_when_delim_present() {
        let columns = vec![col("a"), col("b")];
        let rows = vec![Row {
            cells: vec![
                ColumnValue::Text("hi,there".to_string()),
                ColumnValue::Integer(3),
            ],
        }];
        let out = format_csv(
            &[ExportPart {
                table: None,
                label: None,
                columns: &columns,
                rows: &rows,
            }],
            CsvDelimiter::Comma,
        );
        assert!(out.contains("\"hi,there\""));
        assert!(out.contains(",3"));
    }

    #[test]
    fn json_single_part_is_array() {
        let columns = vec![col("a")];
        let rows = vec![Row {
            cells: vec![ColumnValue::Integer(1)],
        }];
        let out = format_json(&[ExportPart {
            table: None,
            label: None,
            columns: &columns,
            rows: &rows,
        }]);
        assert!(out.trim_start().starts_with('['));
    }

    #[test]
    fn json_multi_part_is_object() {
        let columns = vec![col("a")];
        let rows: Vec<Row> = vec![];
        let out = format_json(&[
            ExportPart {
                table: None,
                label: Some("T1".into()),
                columns: &columns,
                rows: &rows,
            },
            ExportPart {
                table: None,
                label: Some("T2".into()),
                columns: &columns,
                rows: &rows,
            },
        ]);
        assert!(out.contains("\"T1\""));
        assert!(out.contains("\"T2\""));
    }

    #[test]
    fn sql_emits_insert_with_quoted_idents() {
        let columns = vec![col("ID"), col("NAME")];
        let rows = vec![Row {
            cells: vec![
                ColumnValue::Integer(1),
                ColumnValue::Text("o'malley".to_string()),
            ],
        }];
        let out = format_sql(
            &[ExportPart {
                table: None,
                label: Some("USERS".into()),
                columns: &columns,
                rows: &rows,
            }],
            true,
        );
        assert!(out.contains("INSERT INTO \"USERS\""));
        assert!(out.contains("\"ID\""));
        assert!(out.contains("'o''malley'"));
    }

    #[test]
    fn sql_omits_ddl_when_include_ddl_false() {
        use plamenix_types::TableKind;
        let columns = vec![col("ID")];
        let rows = vec![Row {
            cells: vec![ColumnValue::Integer(1)],
        }];
        let table = TableInfo {
            name: "T".into(),
            kind: TableKind::Table,
            columns: vec![ColumnInfo {
                name: "ID".into(),
                position: 0,
                sql_type: "INTEGER".into(),
                nullable: false,
                default_expr: None,
            }],
            primary_key: vec!["ID".into()],
        };
        let with_ddl = format_sql(
            &[ExportPart {
                table: Some(&table),
                label: Some("T".into()),
                columns: &columns,
                rows: &rows,
            }],
            true,
        );
        let no_ddl = format_sql(
            &[ExportPart {
                table: Some(&table),
                label: Some("T".into()),
                columns: &columns,
                rows: &rows,
            }],
            false,
        );
        assert!(with_ddl.contains("CREATE TABLE"));
        assert!(!no_ddl.contains("CREATE TABLE"));
        assert!(no_ddl.contains("INSERT INTO"));
    }

    #[test]
    fn xml_single_part_uses_rows_root() {
        let columns = vec![col("a")];
        let rows = vec![Row {
            cells: vec![ColumnValue::Text("x".into())],
        }];
        let out = format_xml(&[ExportPart {
            table: None,
            label: None,
            columns: &columns,
            rows: &rows,
        }]);
        assert!(out.contains("<rows>"));
        assert!(out.contains("<col name=\"a\">x</col>"));
    }
}
