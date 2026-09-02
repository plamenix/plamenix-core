//! A plugin that imports more than its manifest declares.
//!
//! Its `manifest.toml` says `plugin-minimal`, whose entire point is
//! that it imports nothing but `host`. This targets `plugin-db-reader`
//! and calls `db.current-session`, so the component genuinely carries a
//! `plamenix:plugin/db` import.
//!
//! The host builds a plugin's linker from its *declared* world, so this
//! component should fail to instantiate rather than fail on first use.
//! That is the difference between the capability model being enforced
//! by construction and being enforced by remembering to check — and it
//! is the property `tests/world_enforcement.rs` exercises with this
//! fixture.
//!
//! Nothing about this plugin is malicious. It is what an honest author
//! gets when they add a feature and forget to raise the world in the
//! manifest, which is exactly the case the refusal has to explain well.

wit_bindgen::generate!({
    path: "wit",
    world: "plugin-db-reader",
});

use crate::exports::plamenix::plugin::plugin::{Activation, Guest, Interception};
use crate::plamenix::plugin::db::current_session;
use crate::plamenix::plugin::host::{LogLevel, log};

struct OverReachingPlugin;

impl Guest for OverReachingPlugin {
    fn activate() -> Activation {
        // The import that its declared world does not expose. Reaching
        // for it at activation rather than lazily keeps the failure at
        // instantiation, where the host can explain it.
        match current_session() {
            Ok(Some(session)) => log(LogLevel::Info, &format!("session {session}")),
            Ok(None) => log(LogLevel::Info, "no session"),
            Err(_) => log(LogLevel::Warn, "db refused"),
        }
        Activation::Ok
    }

    fn deactivate() {}

    fn handle_event(_topic: String, _payload: String) {}

    fn intercept(_point: String, _context_json: String) -> Interception {
        Interception::Proceed
    }
}

export!(OverReachingPlugin);
