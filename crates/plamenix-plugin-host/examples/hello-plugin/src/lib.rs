//! Hello-world Plamenix plugin.
//!
//! Reads the host's version via the imported `host-version`, then emits
//! a single info-level log line that interpolates it. Built for
//! `wasm32-wasip2` as a Component Model component via `wit-bindgen`.

wit_bindgen::generate!({
    path: "wit",
    world: "plamenix-plugin",
    generate_all,
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
}

export!(HelloPlugin);
