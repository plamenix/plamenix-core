//! A plugin that actually reads the database.
//!
//! The point of the fixture is to prove the whole path works from the
//! guest's side: declared world, linked interface, capability check,
//! the host's service adapter, and the value coming back through WIT
//! with its type intact. Every layer of that was written separately and
//! this is the first thing that crosses all of them.
//!
//! It logs what it read, because the host's log sink is the only way to
//! see from outside the sandbox what a plugin actually received. A test
//! that asserted only "the call returned Ok" would pass against a
//! plugin that ignored the rows.
//!
//! It also ships as a bundled plugin, so the capability model is
//! visible in the running app: it declares `db.read.any`, the
//! permissions panel shows that as pending until the user approves it,
//! and until then every query it makes is refused.

wit_bindgen::generate!({
    path: "wit",
    world: "plugin-db-reader",
});

use crate::exports::plamenix::plugin::plugin::{Activation, Guest, Interception};
use crate::plamenix::plugin::db::{ColumnValue, current_session, query_read};
use crate::plamenix::plugin::host::{LogLevel, log};

struct DbReaderPlugin;

/// Renders a cell the way a plugin author would see it.
///
/// `Decimal` is spelled out separately from `Float` on purpose: if the
/// host ever routed NUMERIC through a double, this would print
/// `float(...)` and the test asserting on `decimal(...)` would fail.
fn describe(cell: &ColumnValue) -> String {
    match cell {
        ColumnValue::Null => "null".to_owned(),
        ColumnValue::Text(text) => format!("text({text})"),
        ColumnValue::Integer(value) => format!("integer({value})"),
        ColumnValue::Decimal(text) => format!("decimal({text})"),
        ColumnValue::Float(value) => format!("float({value})"),
        ColumnValue::Boolean(value) => format!("boolean({value})"),
        ColumnValue::BlobRef(blob) => format!("blob({})", blob.id),
    }
}

impl Guest for DbReaderPlugin {
    fn activate() -> Activation {
        Activation::Ok
    }

    fn deactivate() {}

    fn handle_event(_topic: String, _payload: String) {
        let session = match current_session() {
            Ok(Some(session)) => session,
            Ok(None) => {
                log(LogLevel::Warn, "db-reader: no session");
                return;
            }
            Err(_) => {
                log(LogLevel::Warn, "db-reader: session refused");
                return;
            }
        };

        // Counts user tables. Chosen because it runs against any
        // Firebird database without knowing its schema, reads nothing
        // the user did not already have on screen, and produces a
        // number a person can sanity-check against the object tree.
        match query_read(
            &session,
            "SELECT COUNT(*) FROM RDB$RELATIONS WHERE RDB$SYSTEM_FLAG = 0",
        ) {
            Ok(result) => {
                let mut message = String::from("db-reader read ");
                message.push_str(&result.rows.len().to_string());
                message.push_str(" row(s):");
                for row in &result.rows {
                    for cell in &row.cells {
                        message.push(' ');
                        message.push_str(&describe(cell));
                    }
                }
                log(LogLevel::Info, &message);
            }
            Err(_) => log(LogLevel::Warn, "db-reader: query refused"),
        }
    }

    fn intercept(_point: String, _context_json: String) -> Interception {
        Interception::Proceed
    }
}

export!(DbReaderPlugin);
