//! The host side of every WIT import a plugin can call.
//!
//! One module per interface. They all follow the same three steps, and
//! the order matters:
//!
//! 1. **Ask [`crate::gate`]** whether the plugin may do this at all.
//!    A refusal is returned as the interface's own error variant, never
//!    as a trap — see the gate's module docs for why trapping on a
//!    policy decision would get plugins disabled for asking.
//! 2. **Do the work**, bounded. Either inside this crate (`fs`,
//!    `settings`, `event-bus`) or through
//!    [`crate::services::HostServices`] for anything only the shell can
//!    reach.
//! 3. **Charge the time back** to the plugin's epoch budget via
//!    [`bounded`], so waiting on the host does not get it preempted.
//!
//! The `host` interface itself (`log`, `host-version`, `edition`) stays
//! in [`crate::host_impl`]: it is ungated, universally available, and
//! predates all of this.

pub mod db;
pub mod fs;
pub mod misc;
pub mod settings;

use std::time::{Duration, Instant};

use crate::host_impl::HostState;
use crate::services::ServiceError;

/// How long a database import may take before the host gives up.
///
/// Generous, because a real query against a real table is not fast, and
/// the epoch budget is repaid so a slow query does not cost the plugin
/// its deadline. What this stops is the case the epoch cannot: a driver
/// call that never returns holds the plugin's store forever, and no
/// deadline fires because no wasm is executing.
pub const DB_IMPORT_BUDGET: Duration = Duration::from_secs(5);

/// How long an outbound HTTP import may take.
pub const NET_IMPORT_BUDGET: Duration = Duration::from_secs(15);

/// How long a keyring import may take.
///
/// Much longer than the others on purpose: on macOS and under Secret
/// Service the first read of an entry can put a password prompt in
/// front of the user, and timing that out would be timing out a human.
pub const KEYRING_IMPORT_BUDGET: Duration = Duration::from_secs(30);

/// Rows a plugin's read query may return before the host truncates.
///
/// Matches the cap both shells already apply to user-run statements.
/// Without it a plugin could ask for a table scan and materialise the
/// whole result in host memory — outside the wasm `ResourceLimiter`,
/// which only sees the guest's linear memory.
pub const PLUGIN_ROW_CAP: u32 = 10_000;

/// Runs one shell-provided call with a deadline, and repays the time to
/// the plugin's epoch budget.
///
/// Both halves matter. The timeout bounds a host that hangs; the charge
/// stops the plugin being preempted for a host that is merely slow.
pub(crate) async fn bounded<T, F>(
    state: &mut HostState,
    budget: Duration,
    future: F,
) -> Result<T, ServiceError>
where
    F: std::future::Future<Output = Result<T, ServiceError>>,
{
    let started = Instant::now();
    let result = tokio::time::timeout(budget, future).await;
    state.charge_host_time(started.elapsed());
    match result {
        Ok(inner) => inner,
        Err(_) => Err(ServiceError::TimedOut),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[tokio::test]
    async fn a_host_call_that_never_returns_is_cut_off() {
        // The epoch deadline cannot save us here: it only fires while
        // wasm is executing, and during a host import none is. Without
        // this bound a hung driver call holds the plugin's store — and
        // therefore the plugin — forever.
        let mut state = crate::host_impl::HostState::new("dev.plamenix.test", "1.0.0-beta");
        let outcome: Result<(), _> = bounded(
            &mut state,
            Duration::from_millis(30),
            std::future::pending::<Result<(), ServiceError>>(),
        )
        .await;

        assert_eq!(outcome, Err(ServiceError::TimedOut));
    }

    #[tokio::test]
    async fn time_spent_waiting_is_charged_back_to_the_plugin() {
        // The other half. A plugin must not be preempted, or have crash
        // budget charged, because the host was slow.
        let mut state = crate::host_impl::HostState::new("dev.plamenix.test", "1.0.0-beta");
        assert_eq!(state.host_import_ticks, 0);

        let _: Result<(), ServiceError> = bounded(&mut state, Duration::from_secs(5), async {
            tokio::time::sleep(Duration::from_millis(45)).await;
            Ok(())
        })
        .await;

        assert!(
            state.host_import_ticks >= 4,
            "45ms should repay at least four ten-millisecond ticks, got {}",
            state.host_import_ticks,
        );
    }

    #[tokio::test]
    async fn a_call_that_timed_out_is_still_charged() {
        // Otherwise the slowest possible host call is the one that
        // repays nothing, which is exactly backwards.
        let mut state = crate::host_impl::HostState::new("dev.plamenix.test", "1.0.0-beta");
        let _: Result<(), ServiceError> = bounded(
            &mut state,
            Duration::from_millis(40),
            std::future::pending::<Result<(), ServiceError>>(),
        )
        .await;

        assert!(
            state.host_import_ticks >= 3,
            "a timed-out call still cost the plugin time"
        );
    }
}
