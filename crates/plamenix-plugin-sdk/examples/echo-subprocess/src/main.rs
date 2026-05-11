//! Reference subprocess plugin.
//!
//! Demonstrates the full activation handshake with the smallest
//! possible payload: log the plugin id + permissions to stderr, then
//! report `Activation::Ok` to the host.

use std::process::ExitCode;

use plamenix_plugin_sdk::subprocess::{Activation, Context, log_info, run};

fn main() -> ExitCode {
    run(|ctx: &Context| {
        log_info(format!(
            "activating {} on host {} with {} permission(s)",
            ctx.plugin_id,
            ctx.host_version,
            ctx.permissions.len()
        ));
        Activation::Ok
    })
}
