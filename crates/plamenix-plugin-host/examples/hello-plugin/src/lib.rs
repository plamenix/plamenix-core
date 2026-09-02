//! Hello-world Plamenix plugin.
//!
//! Reads the host's version via the imported `host-version`, then emits
//! a single info-level log line that interpolates it. Built for
//! `wasm32-wasip2` as a Component Model component via `wit-bindgen`.

wit_bindgen::generate!({
    path: "wit",
    world: "plugin-minimal",
});

use crate::exports::plamenix::plugin::plugin::{Activation, Guest, Interception};
use crate::plamenix::plugin::host::{LogLevel, host_version, log};

struct HelloPlugin;

impl Guest for HelloPlugin {
    fn activate() -> Activation {
        let version = host_version();
        let mut message = String::from("hello from plugin, host is ");
        message.push_str(&version);
        log(LogLevel::Info, &message);
        Activation::Ok
    }

    fn deactivate() {
        log(LogLevel::Info, "hello plugin deactivating");
    }

    fn handle_event(topic: String, payload: String) {
        // Logs both topic and payload — sufficient for the I6.2
        // integration test (host's log sink asserts the line).
        let mut message = String::from("hello plugin received event: ");
        message.push_str(&topic);
        message.push_str(" payload=");
        message.push_str(&payload);
        log(LogLevel::Info, &message);
    }

    /// Exercises all three interception verbs so the host has
    /// something real to test the chain against.
    ///
    /// The behaviour is deliberately the shape a genuine plugin would
    /// have rather than a switch on a magic token: refuse destructive
    /// SQL, rewrite a buffer on save, and stay out of the way
    /// otherwise. The substring matching is the part that is fixture-
    /// grade — a real plugin would parse the JSON, but pulling a JSON
    /// crate into a guest to look at one field is not worth the
    /// bytes here.
    fn intercept(point: String, context_json: String) -> Interception {
        let mut message = String::from("hello plugin intercepting: ");
        message.push_str(&point);
        log(LogLevel::Info, &message);

        match point.as_str() {
            "query.executing" if contains_ignore_case(&context_json, "drop table") => {
                Interception::Cancel(
                    "hello-plugin refuses DROP TABLE while it is installed".to_owned(),
                )
            }
            "editor.saving" => {
                // The formatter-plugin shape: hand back a context the
                // rest of the chain and the host will use instead.
                Interception::Replace(annotate(&context_json))
            }
            _ => Interception::Proceed,
        }
    }
}

/// Case-insensitive substring test without pulling in a dependency.
fn contains_ignore_case(haystack: &str, needle_lowercase: &str) -> bool {
    haystack.to_lowercase().contains(needle_lowercase)
}

/// Appends a marker to the context's `buffer` field, if it has one.
///
/// String surgery rather than JSON parsing, for the same reason as
/// above. Returns the context unchanged when the field is absent, which
/// makes the replace a no-op rather than a corruption.
fn annotate(context_json: &str) -> String {
    const NEEDLE: &str = "\"buffer\":\"";
    let Some(start) = context_json.find(NEEDLE) else {
        return context_json.to_owned();
    };
    let value_start = start + NEEDLE.len();
    let Some(relative_end) = context_json[value_start..].find('"') else {
        return context_json.to_owned();
    };
    let end = value_start + relative_end;
    let mut out = String::with_capacity(context_json.len() + 16);
    out.push_str(&context_json[..end]);
    out.push_str(" -- formatted");
    out.push_str(&context_json[end..]);
    out
}

export!(HelloPlugin);
