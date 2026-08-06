//! Hello-world Plamenix plugin.
//!
//! Reads the host's version via the imported `host-version`, then emits
//! a single info-level log line that interpolates it. Built for
//! `wasm32-wasip2` as a Component Model component via `wit-bindgen`.

wit_bindgen::generate!({
    path: "wit",
    world: "plugin-minimal",
});

use crate::exports::plamenix::plugin::plugin::{Activation, Guest};
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
}

export!(HelloPlugin);
