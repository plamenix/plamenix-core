//! A plugin that misbehaves on purpose.
//!
//! The `hello-plugin` fixture logs and returns, so every host guarantee
//! about *bad* plugins was proven by construction rather than
//! observation: preemption was tested by setting an epoch deadline by
//! hand, the crash budget by calling `on_exit` directly, and the memory
//! cap by checking arithmetic. None of that involves a plugin actually
//! misbehaving, and a guarantee that has never been fired is a
//! guarantee nobody has seen work.
//!
//! One component covers all three, selected by the event topic so the
//! host can pick a behaviour per call without needing a separate
//! bundle for each:
//!
//! | Topic            | Behaviour                                     |
//! |------------------|-----------------------------------------------|
//! | `misbehave/trap` | Traps immediately (unreachable).              |
//! | `misbehave/loop` | Spins forever; only preemption ends it.       |
//! | `misbehave/grow` | Allocates until the memory cap refuses it.    |
//! | anything else    | Behaves, so the fixture can prove a control.  |
//!
//! Built for `wasm32-wasip2` as a Component Model component.

wit_bindgen::generate!({
    path: "wit",
    world: "plugin-minimal",
});

use crate::exports::plamenix::plugin::plugin::{Activation, Guest};
use crate::plamenix::plugin::host::{LogLevel, log};

struct MisbehavingPlugin;

impl Guest for MisbehavingPlugin {
    fn activate() -> Activation {
        log(LogLevel::Info, "misbehaving plugin activated");
        Activation::Ok
    }

    fn deactivate() {}

    fn handle_event(topic: String, _payload: String) {
        match topic.as_str() {
            "misbehave/trap" => {
                // A wasm trap, which is what a panicking or
                // out-of-bounds plugin looks like to the host.
                unreachable!("this plugin traps on purpose");
            }
            "misbehave/loop" => {
                // Never returns. The host's epoch deadline is the only
                // thing that can end this call, so if preemption is not
                // working the test hangs rather than failing — which is
                // itself the signal.
                //
                // `black_box` and a volatile read keep the optimiser
                // from proving the loop is side-effect free and
                // deleting it.
                let mut spin: u64 = 0;
                loop {
                    spin = spin.wrapping_add(1);
                    core::hint::black_box(spin);
                }
            }
            "misbehave/grow" => {
                // Grow linear memory until the host's ResourceLimiter
                // refuses. Each chunk is kept alive so the allocator
                // cannot reuse the same pages.
                let mut held: Vec<Vec<u8>> = Vec::new();
                loop {
                    let mut chunk = vec![0u8; 4 * 1024 * 1024];
                    // Touch both ends so the pages are really committed
                    // rather than lazily mapped.
                    let last = chunk.len() - 1;
                    chunk[0] = 1;
                    chunk[last] = 1;
                    held.push(chunk);
                    core::hint::black_box(&held);
                }
            }
            _ => {
                log(LogLevel::Info, "misbehaving plugin behaved");
            }
        }
    }
}

export!(MisbehavingPlugin);
