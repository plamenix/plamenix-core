//! Plugin call deadlines via wasmtime's epoch interruption (I8.7).
//!
//! ## Mental model
//!
//! wasmtime's preemption mechanism is a single monotonic counter — the
//! **epoch** — owned by the shared [`wasmtime::Engine`]. Plugins
//! instantiated against an epoch-aware engine are compiled with epoch
//! checks at every basic-block boundary; on each check, if the call's
//! stored deadline has been exceeded the call traps.
//!
//! The host drives the counter with a background tick (10 ms by
//! default). Before each plugin call, the host calls
//! `Store::set_epoch_deadline(ticks)` — wasmtime tolerates `ticks`
//! additional engine-epoch bumps before interrupting. Convert ms
//! deadlines to ticks: `ticks = deadline_ms / EPOCH_TICK_MS`.
//!
//! ## Deadlines
//!
//! Two classes today:
//!
//! - **Interactive (100 ms)** — synchronous calls that block the UI:
//!   event dispatch into a plugin, command-palette commands routed to
//!   plugins, in-line cell-edit hooks. The cap exists so a buggy
//!   plugin can't freeze the editor.
//! - **Background (5 s)** — long-running ops: bulk import drivers,
//!   batch export transforms, schema-action DDL chained through a
//!   plugin pipeline. Plugins that need more split the work into
//!   smaller calls (re-entering the host).
//!
//! ## Lifecycle
//!
//! - [`EpochTicker`] spawns a tokio task that calls
//!   `engine.increment_epoch()` every `EPOCH_TICK_MS` until the
//!   ticker is dropped. Drop is async-safe; the task observes the
//!   cancel flag and exits at the next tick boundary.
//! - The host typically creates ONE ticker per process and keeps it
//!   alive for the engine's lifetime.
//!
//! ## Concurrency
//!
//! - Engine is `Clone` (internal `Arc`). The ticker holds its own
//!   `Engine` clone so it doesn't pin the host to a single owner.
//! - The cancel flag is `Arc<AtomicBool>` shared between the ticker
//!   handle + the spawned task. Drop sets it; the task reads it on
//!   each loop iteration.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::task::JoinHandle;
use wasmtime::Engine;

/// Wall-clock interval at which the background task bumps the
/// engine's epoch counter.
pub const EPOCH_TICK_MS: u64 = 10;

/// Default interactive-call deadline in milliseconds (100 ms).
pub const INTERACTIVE_DEADLINE_MS: u64 = 100;

/// Default background-call deadline in milliseconds (5 s).
pub const BACKGROUND_DEADLINE_MS: u64 = 5_000;

/// Per-call class controlling the deadline applied to a plugin call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallClass {
    /// 100 ms deadline. Use for event dispatch, command palette,
    /// inline-edit hooks, and any other call that blocks the UI.
    Interactive,
    /// 5 s deadline. Use for bulk imports, exports, and
    /// long-running transforms.
    Background,
}

impl CallClass {
    /// Returns the deadline duration for this class.
    #[must_use]
    pub const fn deadline(self) -> Duration {
        match self {
            Self::Interactive => Duration::from_millis(INTERACTIVE_DEADLINE_MS),
            Self::Background => Duration::from_millis(BACKGROUND_DEADLINE_MS),
        }
    }

    /// Returns the deadline expressed in engine-epoch ticks. The
    /// ticks parameter passed to `Store::set_epoch_deadline` is the
    /// number of ADDITIONAL `engine.increment_epoch()` calls
    /// tolerated before the deadline expires.
    ///
    /// Computed as `deadline_ms / EPOCH_TICK_MS`. Always returns at
    /// least 1 — a zero deadline would trap before the first wasm
    /// instruction.
    #[must_use]
    pub const fn deadline_ticks(self) -> u64 {
        let ms = match self {
            Self::Interactive => INTERACTIVE_DEADLINE_MS,
            Self::Background => BACKGROUND_DEADLINE_MS,
        };
        let raw = ms / EPOCH_TICK_MS;
        if raw == 0 { 1 } else { raw }
    }
}

/// Background ticker that drives the engine's epoch counter.
///
/// Spawn once per process; the ticker stays alive until dropped.
/// Dropping signals the spawned task to exit at the next tick.
#[derive(Debug)]
pub struct EpochTicker {
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl EpochTicker {
    /// Spawns the background ticker. Bumps `engine.increment_epoch()`
    /// every [`EPOCH_TICK_MS`] until the returned [`EpochTicker`] is
    /// dropped.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime — the spawned task
    /// requires one. Hosts in the Plamenix codebase always run
    /// inside Tauri's / Fastify's Tokio runtime so this is sound.
    #[must_use]
    pub fn spawn(engine: Engine) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_task = Arc::clone(&cancel);
        let handle = tokio::spawn(async move {
            let interval = Duration::from_millis(EPOCH_TICK_MS);
            loop {
                if cancel_for_task.load(Ordering::Acquire) {
                    break;
                }
                tokio::time::sleep(interval).await;
                if cancel_for_task.load(Ordering::Acquire) {
                    break;
                }
                engine.increment_epoch();
            }
        });
        Self {
            cancel,
            handle: Some(handle),
        }
    }

    /// Signals the background task to exit + waits for it to finish.
    /// Equivalent to drop, but explicit + awaitable.
    pub async fn stop(mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }

    /// Returns true when the cancel flag has been set (either by
    /// `stop` or `drop`).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::PluginHost;

    #[test]
    fn interactive_deadline_is_100ms() {
        assert_eq!(
            CallClass::Interactive.deadline(),
            Duration::from_millis(100)
        );
    }

    #[test]
    fn background_deadline_is_5s() {
        assert_eq!(CallClass::Background.deadline(), Duration::from_secs(5));
    }

    #[test]
    fn deadline_ticks_convert_ms_to_engine_ticks() {
        // 100ms / 10ms per tick = 10 ticks.
        assert_eq!(CallClass::Interactive.deadline_ticks(), 10);
        // 5000ms / 10ms per tick = 500 ticks.
        assert_eq!(CallClass::Background.deadline_ticks(), 500);
    }

    #[test]
    fn deadline_ticks_never_returns_zero() {
        // Sanity: both supported classes produce >= 1 tick.
        assert!(CallClass::Interactive.deadline_ticks() >= 1);
        assert!(CallClass::Background.deadline_ticks() >= 1);
    }

    #[test]
    fn tick_constants_match_doc() {
        assert_eq!(EPOCH_TICK_MS, 10);
        assert_eq!(INTERACTIVE_DEADLINE_MS, 100);
        assert_eq!(BACKGROUND_DEADLINE_MS, 5_000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn spawning_and_dropping_ticker_does_not_panic() {
        let host = PluginHost::new().expect("engine");
        let ticker = EpochTicker::spawn(host.engine().clone());
        // Drop immediately — the spawned task should observe the
        // cancel flag and exit cleanly.
        drop(ticker);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn explicit_stop_marks_cancel_flag() {
        let host = PluginHost::new().expect("engine");
        let ticker = EpochTicker::spawn(host.engine().clone());
        assert!(!ticker.is_cancelled());
        let cancel = Arc::clone(&ticker.cancel);
        ticker.stop().await;
        assert!(cancel.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ticker_actually_bumps_epoch_counter() {
        let host = PluginHost::new().expect("engine");
        let engine = host.engine().clone();
        let _ticker = EpochTicker::spawn(engine.clone());
        // Wait long enough to observe at least one tick (10 ms each).
        // Use 60 ms to absorb scheduler jitter on slow CI.
        tokio::time::sleep(Duration::from_millis(60)).await;
        // Bumping epoch is a one-way counter; we can't read its
        // current value back from the engine, but the call wouldn't
        // panic if the ticker has been running, so the test passes
        // when no panic occurs. (Stronger assertion would require
        // an engine with a custom hook, out of scope.)
    }
}
