//! Row, column, and result types returned by [`crate::DbDriver`].

use serde::{Deserialize, Serialize};
use specta::Type;

/// Handle to a binary BLOB cached server-side. The full body is
/// fetched on demand via [`crate::DbDriver::fetch_blob`]; the row
/// payload only carries this small reference so large BLOBs do not
/// inflate every result row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
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
// PartialEq so callers (and tests) can compare cell values directly.
// Derivable because the only float variant is genuinely approximate;
// exact values are carried as text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Type)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ColumnValue {
    /// SQL `NULL` for this column on this row.
    Null,
    /// Textual data (CHAR, VARCHAR, BLOB `SUB_TYPE` TEXT).
    Text(String),
    /// 64-bit signed integer (SMALLINT, INTEGER, BIGINT).
    ///
    /// Carried as decimal text: the magnitude is the user's data, and a
    /// `BIGINT` past 2^53 cannot survive a JSON number. See
    /// [`plamenix_types::exact_int`].
    Integer(
        #[serde(with = "plamenix_types::exact_int")]
        #[specta(type = String)]
        i64,
    ),
    /// Exact fixed-point value (NUMERIC, DECIMAL).
    ///
    /// Carried as decimal text because neither `f64` nor a JSON number
    /// can hold it: Firebird stores these as a scaled 64-bit integer,
    /// and NUMERIC(18,4) — the usual money type — runs past what a
    /// double represents exactly.
    Decimal(String),
    /// Double-precision floating point (FLOAT, DOUBLE PRECISION).
    ///
    /// Genuinely approximate, unlike [`Self::Decimal`]: `f64` is the
    /// faithful representation of what Firebird stores.
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

/// First keyword of a statement, upper-cased, ignoring leading
/// whitespace.
fn leading_keyword(sql: &str) -> Option<String> {
    sql.trim_start()
        .split_whitespace()
        .next()
        .map(|word| word.to_ascii_uppercase())
}

/// How a statement returns its results, which decides the driver call
/// used to run it.
///
/// Firebird does not have one "gives you rows" category. `EXECUTE
/// PROCEDURE` hands back a single row of output parameters through
/// `isc_dsql_execute2`, not a cursor, so fetching it like a `SELECT`
/// fails with "Cursor is not open" — confirmed against Firebird 5.0.4.
/// Collapsing the two into a single predicate is what left procedure
/// output silently discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatementShape {
    /// Produces a cursor to iterate: `SELECT`, `WITH`, `EXECUTE BLOCK`.
    Cursor,
    /// Produces one row of output parameters: `EXECUTE PROCEDURE`.
    OutputParams,
    /// Produces no result set: DML, DDL, everything else.
    NoResultSet,
}

/// Whether an `EXECUTE BLOCK` declares output, and so produces a
/// cursor.
///
/// Only the header is inspected — everything before the `AS` that opens
/// the body — because the body may contain the word in a string literal
/// or a nested statement, and a block that merely mentions `RETURNS`
/// does not return anything.
fn execute_block_returns(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    let header_end = upper
        .match_indices("AS")
        .find(|(idx, _)| {
            let before_ok = *idx == 0
                || !upper.as_bytes()[idx - 1].is_ascii_alphanumeric()
                    && upper.as_bytes()[idx - 1] != b'_';
            let after = idx + 2;
            let after_ok = after >= upper.len()
                || !upper.as_bytes()[after].is_ascii_alphanumeric()
                    && upper.as_bytes()[after] != b'_';
            before_ok && after_ok
        })
        .map_or(upper.len(), |(idx, _)| idx);

    upper[..header_end]
        .match_indices("RETURNS")
        .any(|(idx, _)| {
            let before_ok = idx == 0
                || !upper.as_bytes()[idx - 1].is_ascii_alphanumeric()
                    && upper.as_bytes()[idx - 1] != b'_';
            let after = idx + "RETURNS".len();
            let after_ok = after >= header_end
                || !upper.as_bytes()[after].is_ascii_alphanumeric()
                    && upper.as_bytes()[after] != b'_';
            before_ok && after_ok
        })
}

/// Classifies a statement by how its results come back.
#[must_use]
pub fn statement_shape(sql: &str) -> StatementShape {
    let trimmed = sql.trim_start();
    let mut words = trimmed.split_whitespace();
    let Some(first) = words.next().map(str::to_ascii_uppercase) else {
        return StatementShape::NoResultSet;
    };
    match first.as_str() {
        "SELECT" | "WITH" => StatementShape::Cursor,
        "EXECUTE" => {
            let second = words.next().map(str::to_ascii_uppercase);
            if second.as_deref() == Some("PROCEDURE") {
                StatementShape::OutputParams
            } else if execute_block_returns(trimmed) {
                StatementShape::Cursor
            } else {
                // `EXECUTE BLOCK AS BEGIN ... END` returns nothing.
                // Treating every block as a cursor made the driver ask
                // for one that was never opened, and Firebird answered
                // `-504 Invalid cursor reference / Cursor is not open` —
                // for a statement that had in fact run and committed.
                StatementShape::NoResultSet
            }
        }
        _ => StatementShape::NoResultSet,
    }
}

/// Returns `true` when a `ROWS` clause can be appended to the statement.
///
/// Only `SELECT` and `WITH`. Firebird's `ROWS` is part of the SELECT
/// grammar, so appending it to either `EXECUTE` form is a syntax error
/// — verified against 5.0.4, which rejects both with
/// `SQL error code = -104, Token unknown - ROWS`. The two questions were
/// previously conflated behind one predicate, which is why `EXECUTE`
/// statements were mangled on the way out and then had their rows
/// dropped on the way back.
#[must_use]
pub fn accepts_row_limit(sql: &str) -> bool {
    leading_keyword(sql).is_some_and(|word| matches!(word.as_str(), "SELECT" | "WITH"))
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
    use super::{StatementShape, accepts_row_limit, inject_row_limit, statement_shape};

    #[test]
    fn the_two_execute_forms_return_results_differently() {
        assert_eq!(statement_shape("SELECT * FROM T"), StatementShape::Cursor);
        assert_eq!(
            statement_shape("  select 1 from rdb$database"),
            StatementShape::Cursor
        );
        assert_eq!(
            statement_shape("WITH cte AS (SELECT 1) SELECT * FROM cte"),
            StatementShape::Cursor
        );
        // EXECUTE BLOCK iterates a cursor; EXECUTE PROCEDURE returns one
        // row of output parameters and fails with "Cursor is not open"
        // if fetched like a SELECT.
        assert_eq!(
            statement_shape("EXECUTE BLOCK RETURNS (n INT) AS BEGIN SUSPEND; END"),
            StatementShape::Cursor
        );
        assert_eq!(
            statement_shape("execute procedure SP_ADD(2, 3)"),
            StatementShape::OutputParams
        );
        assert_eq!(
            statement_shape("UPDATE t SET c = 1"),
            StatementShape::NoResultSet
        );
        assert_eq!(statement_shape("DROP TABLE t"), StatementShape::NoResultSet);
        assert_eq!(statement_shape("   "), StatementShape::NoResultSet);
    }

    #[test]
    fn only_select_and_with_tolerate_a_rows_clause() {
        assert!(accepts_row_limit("SELECT * FROM T"));
        assert!(accepts_row_limit(
            "WITH cte AS (SELECT 1) SELECT * FROM cte"
        ));
        // Firebird 5.0.4 rejects both of these with
        // "Token unknown - ROWS"; capping them is a syntax error, not a
        // safety net.
        assert!(!accepts_row_limit(
            "EXECUTE BLOCK RETURNS (n INT) AS BEGIN SUSPEND; END"
        ));
        assert!(!accepts_row_limit("EXECUTE PROCEDURE SP_ADD(2, 3)"));
        assert!(!accepts_row_limit("UPDATE t SET c = 1"));
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

/// Splits a SQL batch into individual statements at the active
/// terminator.
///
/// Honours Firebird string and identifier quoting (`'foo''bar'` and
/// `"id ""esc"""`), line comments (`-- to end of line`), and block
/// comments (`/* ... */`). Comment-only or whitespace-only statements
/// are dropped — they would only confuse the driver.
///
/// Procedural SQL needs two extra rules, without which a batch
/// containing a procedure, trigger, or `EXECUTE BLOCK` is torn apart at
/// the semicolons inside its body and every fragment fails to parse:
///
/// * **`SET TERM`** switches the terminator, exactly as `isql` and
///   IBExpert scripts expect. The directive is consumed rather than
///   emitted — it is a client instruction the server has never heard of.
/// * **`BEGIN` / `END` nesting** means a semicolon inside a PSQL body is
///   just a statement separator within that body, so a block stays whole
///   even when the script never sets a terminator. `CASE` counts as an
///   opener too, since it also closes with `END`.
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
///
/// A block survives intact without any terminator ceremony:
///
/// ```
/// let stmts = plamenix_db::split_statements(
///     "EXECUTE BLOCK RETURNS (N INTEGER) AS BEGIN N = 1; SUSPEND; END;"
/// );
/// assert_eq!(stmts.len(), 1);
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
    // The active statement terminator. `SET TERM` replaces it; `isql`
    // scripts conventionally swap in `^` around a procedure body.
    let mut terminator: String = ";".to_string();
    // Nesting of PSQL blocks. While non-zero the terminator is inert,
    // because the separators inside a body belong to the body.
    let mut depth = 0u32;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match state {
            State::Normal => {
                if !has_content {
                    if let Some((new_term, consumed)) = parse_set_term(sql, i, &terminator) {
                        terminator = new_term;
                        i = consumed;
                        start = i;
                        continue;
                    }
                }
                match c {
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
                    _ => {
                        if depth == 0 && sql[i..].starts_with(terminator.as_str()) {
                            if has_content {
                                out.push(sql[start..i].trim().to_string());
                            }
                            i += terminator.len();
                            start = i;
                            has_content = false;
                            continue;
                        }
                        if let Some(word_len) = keyword_at(sql, bytes, i) {
                            match &sql[i..i + word_len].to_ascii_uppercase()[..] {
                                "BEGIN" | "CASE" => depth += 1,
                                // Saturating: a stray END must not wrap
                                // the counter and disable the terminator
                                // for the rest of the batch.
                                "END" => depth = depth.saturating_sub(1),
                                _ => {}
                            }
                            has_content = true;
                            i += word_len;
                            continue;
                        }
                        if !c.is_ascii_whitespace() {
                            has_content = true;
                        }
                    }
                }
            }
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

/// Length of the identifier-like word starting at `i`, or `None` when
/// `i` is not on a word boundary.
///
/// Used so `ENDING` and `BEGINNER` do not move the block depth.
fn keyword_at(sql: &str, bytes: &[u8], i: usize) -> Option<usize> {
    fn is_word_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
    }
    if !bytes[i].is_ascii_alphabetic() {
        return None;
    }
    if i > 0 && is_word_byte(bytes[i - 1]) {
        return None;
    }
    let len = sql[i..].bytes().take_while(|b| is_word_byte(*b)).count();
    Some(len)
}

/// Recognises a `SET TERM <new> <current>` directive at `i`.
///
/// Returns the new terminator and the offset just past the directive,
/// or `None` when this is ordinary SQL. `SET TERM` is a client-side
/// instruction — the server would reject it — so callers consume it
/// instead of emitting it as a statement.
fn parse_set_term(sql: &str, i: usize, current: &str) -> Option<(String, usize)> {
    let rest = &sql[i..];
    let mut cursor = 0usize;

    let expect_word = |word: &str, cursor: &mut usize| -> bool {
        let tail = &rest[*cursor..];
        let trimmed = tail.trim_start();
        let skipped = tail.len() - trimmed.len();
        if trimmed.len() < word.len() || !trimmed[..word.len()].eq_ignore_ascii_case(word) {
            return false;
        }
        let after = trimmed.as_bytes().get(word.len()).copied();
        if after.is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$') {
            return false;
        }
        *cursor += skipped + word.len();
        true
    };

    if !expect_word("SET", &mut cursor) || !expect_word("TERM", &mut cursor) {
        return None;
    }

    let tail = &rest[cursor..];
    let trimmed = tail.trim_start();
    cursor += tail.len() - trimmed.len();
    let token_len = trimmed
        .bytes()
        .take_while(|b| !b.is_ascii_whitespace())
        .count();
    if token_len == 0 {
        return None;
    }
    let token = &trimmed[..token_len];
    cursor += token_len;

    // The directive is itself closed by the terminator in force before
    // the switch, so `SET TERM ^ ;` reads as "from now on use ^", and
    // `SET TERM ; ^` restores the default. The closing terminator may be
    // written without a space (`SET TERM ^;`), in which case it is part
    // of this token and has to come back off. `;` must stay a legal new
    // terminator, which is what makes the restore form work.
    let new_term = match token.strip_suffix(current) {
        Some(prefix) if !prefix.is_empty() => prefix.to_string(),
        _ => {
            let tail = &rest[cursor..];
            let trimmed = tail.trim_start();
            cursor += tail.len() - trimmed.len();
            if trimmed.starts_with(current) {
                cursor += current.len();
            }
            token.to_string()
        }
    };

    Some((new_term, i + cursor))
}

#[cfg(test)]
mod tests {
    use super::{StatementShape, split_statements, statement_shape};

    /// Reproduces, offline, what the live FB 5.0.4 container showed:
    /// without terminator awareness a block is torn at its internal
    /// semicolons and every fragment fails to parse.
    #[test]
    fn an_execute_block_without_returns_produces_no_cursor() {
        // Every `EXECUTE BLOCK` used to be classified as a cursor. A
        // block with no `RETURNS` returns nothing, so the driver asked
        // for a cursor that was never opened and Firebird answered
        // `-504 Cursor is not open` — for a statement that had actually
        // run and committed. Anyone writing a procedural block in the
        // editor hit it.
        assert_eq!(
            statement_shape("EXECUTE BLOCK AS BEGIN INSERT INTO T VALUES (1); END"),
            StatementShape::NoResultSet,
        );
    }

    #[test]
    fn an_execute_block_with_returns_still_produces_a_cursor() {
        assert_eq!(
            statement_shape("EXECUTE BLOCK RETURNS (N INTEGER) AS BEGIN N = 1; SUSPEND; END"),
            StatementShape::Cursor,
        );
        assert_eq!(
            statement_shape(
                "EXECUTE BLOCK (P INTEGER = ?) RETURNS (N INTEGER) AS BEGIN N = P; SUSPEND; END"
            ),
            StatementShape::Cursor,
        );
    }

    #[test]
    fn the_word_returns_inside_a_block_body_does_not_make_it_a_cursor() {
        // Only the header decides. A body that merely mentions the word
        // — in a string, a comment, or a nested statement — returns
        // nothing.
        assert_eq!(
            statement_shape(
                "EXECUTE BLOCK AS BEGIN INSERT INTO LOG VALUES ('nothing RETURNS here'); END"
            ),
            StatementShape::NoResultSet,
        );
    }

    #[test]
    fn execute_block_survives_without_set_term() {
        let stmts =
            split_statements("EXECUTE BLOCK RETURNS (N INTEGER) AS BEGIN N = 42; SUSPEND; END;");
        assert_eq!(stmts.len(), 1, "block was split: {stmts:?}");
        assert!(stmts[0].starts_with("EXECUTE BLOCK"));
        assert!(stmts[0].ends_with("END"));
    }

    #[test]
    fn set_term_switches_the_terminator_and_is_not_emitted() {
        let stmts = split_statements(
            "SET TERM ^ ;\n\
             CREATE PROCEDURE P AS BEGIN EXIT; END^\n\
             SET TERM ; ^\n\
             SELECT 1;",
        );
        assert_eq!(stmts.len(), 2, "unexpected split: {stmts:?}");
        assert!(stmts[0].starts_with("CREATE PROCEDURE P"));
        assert_eq!(stmts[1], "SELECT 1");
        assert!(
            !stmts.iter().any(|s| s.to_uppercase().contains("SET TERM")),
            "SET TERM leaked to the server: {stmts:?}",
        );
    }

    #[test]
    fn multi_character_terminators_work() {
        let stmts =
            split_statements("SET TERM !! ;\nCREATE TRIGGER T AS BEGIN EXIT; END!!\nSET TERM ; !!");
        assert_eq!(stmts.len(), 1, "unexpected split: {stmts:?}");
        assert!(stmts[0].starts_with("CREATE TRIGGER T"));
    }

    #[test]
    fn nested_blocks_and_case_stay_whole() {
        // CASE also closes with END; counting only BEGIN would let the
        // CASE's END close the outer block and split at the next `;`.
        let stmts = split_statements(
            "EXECUTE BLOCK AS BEGIN \
             IF (1=1) THEN BEGIN X = CASE WHEN A THEN 1 ELSE 2 END; END \
             SUSPEND; END;",
        );
        assert_eq!(stmts.len(), 1, "block was split: {stmts:?}");
    }

    #[test]
    fn a_stray_end_does_not_disable_the_terminator() {
        // Saturating depth: an unmatched END must not wrap the counter
        // and swallow the rest of the batch into one statement.
        let stmts = split_statements("SELECT 1 END; SELECT 2;");
        assert_eq!(stmts.len(), 2, "terminator went inert: {stmts:?}");
    }

    #[test]
    fn block_keywords_inside_strings_and_comments_are_ignored() {
        let stmts = split_statements("SELECT 'BEGIN' FROM T; -- BEGIN\nSELECT /* BEGIN */ 2;");
        assert_eq!(stmts.len(), 2, "unexpected split: {stmts:?}");
    }

    #[test]
    fn words_merely_starting_with_a_keyword_do_not_nest() {
        let stmts = split_statements("SELECT ENDING, BEGINNER FROM T; SELECT 2;");
        assert_eq!(stmts.len(), 2, "word boundary ignored: {stmts:?}");
    }

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
