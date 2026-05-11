//! Subprocess plugin protocol helper.
//!
//! Mirrors the parent-side activator at
//! `plamenix-plugin-host/src/subprocess.rs`. Plugin authors hand
//! [`run`] an activate closure; the helper parses argv, calls the
//! closure with a [`Context`], emits the JSON status line on stdout,
//! and exits with the matching code. Diagnostic logging uses
//! [`log_info`] / [`log_warn`] / [`log_error`], which write to stderr
//! (the host forwards stderr lines to its `tracing` pipeline keyed on
//! the plugin id).
//!
//! ## Quickstart
//!
//! `Cargo.toml`:
//!
//! ```toml
//! [dependencies]
//! plamenix-plugin-sdk = { path = "../../plamenix-core/crates/plamenix-plugin-sdk" }
//! ```
//!
//! `src/main.rs`:
//!
//! ```no_run
//! use plamenix_plugin_sdk::subprocess::{Activation, Context, log_info, run};
//!
//! fn main() -> std::process::ExitCode {
//!     run(|ctx: &Context| {
//!         log_info(format!("activating {} on host {}", ctx.plugin_id, ctx.host_version));
//!         Activation::Ok
//!     })
//! }
//! ```

use std::env;
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

/// Activation outcome returned by the user's activate closure.
///
/// Mirrors the WIT `activation` variant on the WASM side.
#[derive(Clone, Debug)]
pub enum Activation {
    /// Plugin activated successfully.
    Ok,
    /// Plugin reported a user-visible failure with the given message.
    Failed(String),
}

/// Activation arguments forwarded by the host on argv.
#[derive(Clone, Debug)]
pub struct Context {
    /// Plugin id from the manifest, used by the host for log
    /// enrichment and capability checks.
    pub plugin_id: String,
    /// `SemVer` string of the host (e.g. `"1.0.0-beta"`).
    pub host_version: String,
    /// Capability strings the host has granted, in their canonical
    /// dotted form (e.g. `"runtime.subprocess"`, `"net.https.example.com"`).
    pub permissions: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum StatusLine {
    Ok,
    Failed { message: String },
}

/// Entry point for subprocess plugins.
///
/// Call from `main`. Parses argv, invokes `activate`, writes the JSON
/// status line to stdout, and returns the matching [`ExitCode`].
///
/// The closure receives a [`Context`] built from
/// `--plugin-id` / `--host-version` / `--permissions` (a JSON-encoded
/// array of capability strings).
///
/// Malformed argv yields exit code 2 and a `failed` status line.
pub fn run<F>(activate: F) -> ExitCode
where
    F: FnOnce(&Context) -> Activation,
{
    let Ok(ctx) = parse_argv(env::args().collect()) else {
        emit(&StatusLine::Failed {
            message: "missing or malformed argv".into(),
        });
        return ExitCode::from(2);
    };

    let outcome = activate(&ctx);
    match &outcome {
        Activation::Ok => emit(&StatusLine::Ok),
        Activation::Failed(message) => emit(&StatusLine::Failed {
            message: message.clone(),
        }),
    }
    match outcome {
        Activation::Ok => ExitCode::SUCCESS,
        Activation::Failed(_) => ExitCode::from(1),
    }
}

fn emit(line: &StatusLine) {
    if let Ok(text) = serde_json::to_string(line) {
        println!("{text}");
    }
}

fn parse_argv(args: Vec<String>) -> Result<Context, &'static str> {
    let mut activate = false;
    let mut plugin_id = None;
    let mut host_version = None;
    let mut permissions_raw: Option<String> = None;

    let mut it = args.into_iter().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--activate" => activate = true,
            "--plugin-id" => plugin_id = it.next(),
            "--host-version" => host_version = it.next(),
            "--permissions" => permissions_raw = it.next(),
            _ => {}
        }
    }

    if !activate {
        return Err("missing --activate");
    }

    let permissions: Vec<String> = match permissions_raw {
        Some(raw) => serde_json::from_str(&raw).map_err(|_| "malformed --permissions json")?,
        None => Vec::new(),
    };

    Ok(Context {
        plugin_id: plugin_id.ok_or("missing --plugin-id")?,
        host_version: host_version.ok_or("missing --host-version")?,
        permissions,
    })
}

fn log(level: &str, message: &str) {
    eprintln!("[{level}] {message}");
}

/// Writes an info-level diagnostic line to stderr; the host forwards
/// it through `tracing::info!` keyed on the plugin id.
pub fn log_info(message: impl AsRef<str>) {
    log("info", message.as_ref());
}

/// Same as [`log_info`] but at warn level.
pub fn log_warn(message: impl AsRef<str>) {
    log("warn", message.as_ref());
}

/// Same as [`log_info`] but at error level.
pub fn log_error(message: impl AsRef<str>) {
    log("error", message.as_ref());
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parse_argv_basic() {
        let args = vec![
            "/path/to/plugin".into(),
            "--activate".into(),
            "--plugin-id".into(),
            "my.plugin".into(),
            "--host-version".into(),
            "1.0.0-beta".into(),
            "--permissions".into(),
            "[\"runtime.subprocess\",\"net.https\"]".into(),
        ];
        let ctx = parse_argv(args).unwrap();
        assert_eq!(ctx.plugin_id, "my.plugin");
        assert_eq!(ctx.host_version, "1.0.0-beta");
        assert_eq!(ctx.permissions, vec!["runtime.subprocess", "net.https"]);
    }

    #[test]
    fn parse_argv_missing_activate_errors() {
        let args = vec!["/path/to/plugin".into()];
        assert!(parse_argv(args).is_err());
    }

    #[test]
    fn parse_argv_missing_plugin_id_errors() {
        let args = vec![
            "/path/to/plugin".into(),
            "--activate".into(),
            "--host-version".into(),
            "1".into(),
        ];
        assert!(parse_argv(args).is_err());
    }

    #[test]
    fn parse_argv_empty_permissions() {
        let args = vec![
            "/path/to/plugin".into(),
            "--activate".into(),
            "--plugin-id".into(),
            "p".into(),
            "--host-version".into(),
            "1".into(),
        ];
        let ctx = parse_argv(args).unwrap();
        assert!(ctx.permissions.is_empty());
    }

    #[test]
    fn status_line_serialises_ok() {
        let s = serde_json::to_string(&StatusLine::Ok).unwrap();
        assert_eq!(s, r#"{"status":"ok"}"#);
    }

    #[test]
    fn status_line_serialises_failed() {
        let s = serde_json::to_string(&StatusLine::Failed {
            message: "boom".into(),
        })
        .unwrap();
        assert_eq!(s, r#"{"status":"failed","message":"boom"}"#);
    }
}
