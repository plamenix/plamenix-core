//! Plugin supervisor — tracks per-plugin runtime state, restart
//! policy decisions, and the crash budget that prevents thrashing
//! plugins from being restarted in a tight loop.
//!
//! ## Mental model
//!
//! The host's plugin lifecycle goes through five named states:
//!
//! ```text
//!  Loaded → Active ──┬──> Stopped     (user requests deactivation)
//!                    ├──> Crashing    (trap, fuel, host eviction)
//!                    │       │
//!                    │       ├──> Active   (restart within budget)
//!                    │       └──> Disabled (crash budget exhausted)
//!                    └──> Disabled    (Permanent + budget exhausted)
//! ```
//!
//! ## Crash budget
//!
//! Each plugin owns a sliding-window crash budget. The default is
//! **3 crashes in 60 seconds**. Every fresh crash is recorded with a
//! timestamp; the budget answers "is this plugin still within
//! budget?" by counting crashes whose age is < window.
//!
//! When the budget is exhausted, the supervisor transitions the
//! plugin to `Disabled` regardless of its restart policy. A user
//! action (re-enable) is required to leave the disabled state.
//!
//! ## Restart policy
//!
//! `RestartPolicy` from the manifest decides what the supervisor
//! does when a plugin reaches an exit:
//!
//! - `Permanent` — restart on every exit (normal + abnormal). Reserve
//!   for first-party essential plugins.
//! - `Transient` — restart on abnormal exits only (trap, fuel
//!   exhaustion, etc.). Default for community plugins.
//! - `Temporary` — never restart. Use for one-shot tools.
//!
//! The crash budget still applies to `Permanent` plugins — the budget
//! beats the policy.
//!
//! ## Threading
//!
//! The supervisor is `Send + Sync` and the per-plugin state lives
//! behind a `RwLock` so the activator (write) and read-only queries
//! from UI surfaces don't block each other.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::error::PluginError;
use crate::manifest::RestartPolicy;

/// Lifecycle state for a single plugin instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginStatus {
    /// Bundle parsed + manifest validated, not yet instantiated.
    Loaded,
    /// Running; ready to handle calls.
    Active,
    /// Exited (trap, fuel, host eviction); restart decision pending.
    Crashing,
    /// User-requested deactivation; will not restart automatically.
    Stopped,
    /// Crash budget exhausted; refuses activation until re-enabled.
    Disabled,
}

/// Reason a plugin reached an exit state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// Normal exit — the plugin's `deactivate` returned without error.
    /// `Permanent` policy restarts; `Transient`/`Temporary` do not.
    Normal,
    /// Abnormal exit — trap, fuel exhaustion, panic, deadline missed.
    /// `Permanent`/`Transient` policies attempt restart (subject to
    /// crash budget); `Temporary` does not.
    Abnormal,
    /// User-requested deactivation. Never restarts; transitions to
    /// `Stopped` regardless of policy.
    UserRequested,
}

/// Verdict the supervisor returns when a plugin exits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartDecision {
    /// Spin up a fresh store + re-activate.
    Restart,
    /// Leave the plugin in `Stopped` state without restarting.
    Stop,
    /// Transition to `Disabled`; do not restart until the user
    /// re-enables the plugin from the Permissions panel.
    Disable { reason: DisableReason },
}

/// Why a plugin was disabled. Surfaced to the UI for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisableReason {
    /// Crash budget exhausted — too many crashes in the window.
    CrashBudgetExhausted,
}

/// Default crash budget: 3 crashes within 60 seconds.
const DEFAULT_MAX_CRASHES: usize = 3;
const DEFAULT_WINDOW_SECS: u64 = 60;

/// Sliding-window crash budget. Stores crash timestamps; expired
/// entries are pruned lazily on each `record_crash` call.
#[derive(Clone, Debug)]
pub struct CrashBudget {
    max_crashes: usize,
    window: Duration,
    crashes: VecDeque<Instant>,
}

impl CrashBudget {
    /// Builds a budget with the supplied capacity + window.
    #[must_use]
    pub fn new(max_crashes: usize, window: Duration) -> Self {
        Self {
            max_crashes,
            window,
            crashes: VecDeque::with_capacity(max_crashes + 1),
        }
    }

    /// Returns the default budget (3 crashes / 60s).
    #[must_use]
    pub fn default_budget() -> Self {
        Self::new(
            DEFAULT_MAX_CRASHES,
            Duration::from_secs(DEFAULT_WINDOW_SECS),
        )
    }

    /// Records a crash at `now`. Returns whether the budget is now
    /// exhausted (the recorded crash counts toward the limit).
    pub fn record_crash(&mut self, now: Instant) -> CrashBudgetState {
        self.prune(now);
        self.crashes.push_back(now);
        if self.crashes.len() >= self.max_crashes {
            CrashBudgetState::Exhausted
        } else {
            CrashBudgetState::WithinBudget {
                remaining: self.max_crashes - self.crashes.len(),
            }
        }
    }

    /// Crashes recorded within the window as of `now` (without
    /// recording a new crash). Useful for the UI to display
    /// "2 of 3 used".
    #[must_use]
    pub fn count_in_window(&self, now: Instant) -> usize {
        self.crashes
            .iter()
            .filter(|t| now.duration_since(**t) < self.window)
            .count()
    }

    /// Maximum allowed crashes per window.
    #[must_use]
    pub fn max_crashes(&self) -> usize {
        self.max_crashes
    }

    /// Window duration.
    #[must_use]
    pub fn window(&self) -> Duration {
        self.window
    }

    /// Resets the budget. Called when the user re-enables a
    /// `Disabled` plugin.
    pub fn reset(&mut self) {
        self.crashes.clear();
    }

    fn prune(&mut self, now: Instant) {
        while let Some(front) = self.crashes.front() {
            if now.duration_since(*front) >= self.window {
                self.crashes.pop_front();
            } else {
                break;
            }
        }
    }
}

/// Result of `record_crash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashBudgetState {
    /// Crash counted; plugin still allowed to restart.
    WithinBudget { remaining: usize },
    /// Budget exhausted; plugin must transition to `Disabled`.
    Exhausted,
}

/// Per-plugin supervision record.
#[derive(Clone, Debug)]
pub struct PluginSupervisionState {
    pub plugin_id: String,
    pub status: PluginStatus,
    pub restart_policy: RestartPolicy,
    pub crash_budget: CrashBudget,
    pub restart_count: usize,
    /// Set by the host when activation succeeds; cleared on
    /// deactivation. Used by the UI to surface uptime.
    pub last_activated_at: Option<Instant>,
}

impl PluginSupervisionState {
    /// Registers a fresh `Loaded` state with the default budget.
    #[must_use]
    pub fn new(plugin_id: String, restart_policy: RestartPolicy) -> Self {
        Self {
            plugin_id,
            status: PluginStatus::Loaded,
            restart_policy,
            crash_budget: CrashBudget::default_budget(),
            restart_count: 0,
            last_activated_at: None,
        }
    }
}

/// Process-wide plugin supervisor.
#[derive(Clone, Debug, Default)]
pub struct Supervisor {
    inner: Arc<RwLock<HashMap<String, PluginSupervisionState>>>,
}

impl Supervisor {
    /// Builds an empty supervisor. Host code wraps this in `Arc` and
    /// hands clones to the activator + UI surfaces.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a fresh plugin in `Loaded` state. Idempotent: a
    /// second call for the same `plugin_id` updates the restart
    /// policy + resets the budget (covers plugin re-install).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned (a panic happened while a writer held the lock).
    pub fn register(
        &self,
        plugin_id: impl Into<String>,
        restart_policy: RestartPolicy,
    ) -> Result<(), PluginError> {
        let id: String = plugin_id.into();
        let mut guard = self.inner.write().map_err(lock_poisoned)?;
        guard
            .entry(id.clone())
            .and_modify(|state| {
                state.restart_policy = restart_policy;
                state.crash_budget.reset();
                state.status = PluginStatus::Loaded;
                state.restart_count = 0;
                state.last_activated_at = None;
            })
            .or_insert_with(|| PluginSupervisionState::new(id, restart_policy));
        Ok(())
    }

    /// Forgets a plugin entirely (uninstall path). Subsequent calls
    /// for the same id behave as if the plugin had never been seen.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned.
    pub fn forget(&self, plugin_id: &str) -> Result<(), PluginError> {
        let mut guard = self.inner.write().map_err(lock_poisoned)?;
        guard.remove(plugin_id);
        Ok(())
    }

    /// Marks the plugin as `Active` at the given timestamp. Called
    /// by the activator after the WASM instance is instantiated +
    /// the plugin's `activate` returned without error.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::PluginNotFound`] if the id is unknown.
    pub fn mark_active(&self, plugin_id: &str, at: Instant) -> Result<(), PluginError> {
        self.with_state_mut(plugin_id, |state| {
            state.status = PluginStatus::Active;
            state.last_activated_at = Some(at);
        })
    }

    /// Marks the plugin as `Stopped` (user-requested deactivation).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::PluginNotFound`] if the id is unknown.
    pub fn mark_stopped(&self, plugin_id: &str) -> Result<(), PluginError> {
        self.with_state_mut(plugin_id, |state| {
            state.status = PluginStatus::Stopped;
            state.last_activated_at = None;
        })
    }

    /// Re-enables a `Disabled` plugin: resets the crash budget +
    /// restart counter + transitions to `Loaded` so the next
    /// activation can succeed. Surfaced to the UI's Permissions
    /// panel via I8.5.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::PluginNotFound`] if the id is unknown.
    pub fn re_enable(&self, plugin_id: &str) -> Result<(), PluginError> {
        self.with_state_mut(plugin_id, |state| {
            state.crash_budget.reset();
            state.restart_count = 0;
            state.status = PluginStatus::Loaded;
        })
    }

    /// Reports an exit + asks for the restart verdict. Updates the
    /// supervised state in place: records a crash on `Abnormal`,
    /// transitions status to `Crashing` then `Disabled` if the
    /// budget is exhausted.
    ///
    /// `now` is injectable so tests can drive deterministic
    /// timestamps without sleeping.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::PluginNotFound`] if the id is unknown.
    pub fn on_exit(
        &self,
        plugin_id: &str,
        reason: ExitReason,
        now: Instant,
    ) -> Result<RestartDecision, PluginError> {
        let mut guard = self.inner.write().map_err(lock_poisoned)?;
        let state = guard
            .get_mut(plugin_id)
            .ok_or_else(|| PluginError::PluginNotFound(plugin_id.to_string()))?;

        // User-requested deactivation is non-negotiable.
        if reason == ExitReason::UserRequested {
            state.status = PluginStatus::Stopped;
            state.last_activated_at = None;
            return Ok(RestartDecision::Stop);
        }

        // Determine whether the policy permits a restart for this
        // exit kind.
        let policy_allows_restart = match (state.restart_policy, reason) {
            (RestartPolicy::Permanent, _) => true,
            (RestartPolicy::Transient, ExitReason::Abnormal) => true,
            (RestartPolicy::Transient, ExitReason::Normal) => false,
            (RestartPolicy::Temporary, _) => false,
            (_, ExitReason::UserRequested) => false,
        };

        if !policy_allows_restart {
            state.status = PluginStatus::Stopped;
            state.last_activated_at = None;
            return Ok(RestartDecision::Stop);
        }

        // Abnormal exits count against the crash budget. Normal
        // exits that policy chose to restart (Permanent) do NOT
        // count — they're voluntary cycles, not crashes.
        if reason == ExitReason::Abnormal {
            let budget_state = state.crash_budget.record_crash(now);
            if budget_state == CrashBudgetState::Exhausted {
                state.status = PluginStatus::Disabled;
                state.last_activated_at = None;
                return Ok(RestartDecision::Disable {
                    reason: DisableReason::CrashBudgetExhausted,
                });
            }
        }

        state.status = PluginStatus::Crashing;
        state.restart_count = state.restart_count.saturating_add(1);
        state.last_activated_at = None;
        Ok(RestartDecision::Restart)
    }

    /// Returns the current state of a plugin. Returns `None` if the
    /// id is unknown (uninstalled / never registered).
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned.
    pub fn state(&self, plugin_id: &str) -> Result<Option<PluginSupervisionState>, PluginError> {
        let guard = self.inner.read().map_err(lock_poisoned)?;
        Ok(guard.get(plugin_id).cloned())
    }

    /// Returns the status of a plugin (if known). Convenience for
    /// the common UI-poll case.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned.
    pub fn status(&self, plugin_id: &str) -> Result<Option<PluginStatus>, PluginError> {
        let guard = self.inner.read().map_err(lock_poisoned)?;
        Ok(guard.get(plugin_id).map(|s| s.status))
    }

    /// Lists every registered plugin's id + status. Used by the
    /// Permissions panel to render the matrix.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError::Runtime`] if the internal lock is
    /// poisoned.
    pub fn list(&self) -> Result<Vec<(String, PluginStatus)>, PluginError> {
        let guard = self.inner.read().map_err(lock_poisoned)?;
        Ok(guard
            .iter()
            .map(|(id, state)| (id.clone(), state.status))
            .collect())
    }

    fn with_state_mut<F>(&self, plugin_id: &str, mutate: F) -> Result<(), PluginError>
    where
        F: FnOnce(&mut PluginSupervisionState),
    {
        let mut guard = self.inner.write().map_err(lock_poisoned)?;
        let state = guard
            .get_mut(plugin_id)
            .ok_or_else(|| PluginError::PluginNotFound(plugin_id.to_string()))?;
        mutate(state);
        Ok(())
    }
}

fn lock_poisoned<T>(_err: T) -> PluginError {
    PluginError::Runtime("plugin supervisor lock poisoned".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn register_inserts_loaded_state() {
        let sup = Supervisor::new();
        sup.register("p1", RestartPolicy::Transient).unwrap();
        let state = sup.state("p1").unwrap().unwrap();
        assert_eq!(state.status, PluginStatus::Loaded);
        assert_eq!(state.restart_count, 0);
    }

    #[test]
    fn register_twice_resets_budget_and_status() {
        let sup = Supervisor::new();
        sup.register("p1", RestartPolicy::Transient).unwrap();
        sup.mark_active("p1", now()).unwrap();
        let _ = sup.on_exit("p1", ExitReason::Abnormal, now()).unwrap();
        sup.register("p1", RestartPolicy::Permanent).unwrap();
        let state = sup.state("p1").unwrap().unwrap();
        assert_eq!(state.status, PluginStatus::Loaded);
        assert_eq!(state.restart_count, 0);
        assert_eq!(state.restart_policy, RestartPolicy::Permanent);
    }

    #[test]
    fn forget_removes_plugin() {
        let sup = Supervisor::new();
        sup.register("p1", RestartPolicy::Transient).unwrap();
        sup.forget("p1").unwrap();
        assert!(sup.state("p1").unwrap().is_none());
    }

    #[test]
    fn mark_active_transitions_to_active() {
        let sup = Supervisor::new();
        sup.register("p1", RestartPolicy::Transient).unwrap();
        let t = now();
        sup.mark_active("p1", t).unwrap();
        let state = sup.state("p1").unwrap().unwrap();
        assert_eq!(state.status, PluginStatus::Active);
        assert_eq!(state.last_activated_at, Some(t));
    }

    #[test]
    fn user_requested_exit_always_stops() {
        let sup = Supervisor::new();
        for policy in [
            RestartPolicy::Permanent,
            RestartPolicy::Transient,
            RestartPolicy::Temporary,
        ] {
            sup.register("p", policy).unwrap();
            sup.mark_active("p", now()).unwrap();
            let dec = sup.on_exit("p", ExitReason::UserRequested, now()).unwrap();
            assert_eq!(dec, RestartDecision::Stop);
            assert_eq!(sup.status("p").unwrap(), Some(PluginStatus::Stopped));
            sup.forget("p").unwrap();
        }
    }

    #[test]
    fn temporary_policy_never_restarts() {
        let sup = Supervisor::new();
        sup.register("p", RestartPolicy::Temporary).unwrap();
        sup.mark_active("p", now()).unwrap();
        let dec = sup.on_exit("p", ExitReason::Abnormal, now()).unwrap();
        assert_eq!(dec, RestartDecision::Stop);
        let dec = sup.on_exit("p", ExitReason::Normal, now()).unwrap();
        assert_eq!(dec, RestartDecision::Stop);
    }

    #[test]
    fn transient_policy_restarts_only_on_abnormal() {
        let sup = Supervisor::new();
        sup.register("p", RestartPolicy::Transient).unwrap();
        sup.mark_active("p", now()).unwrap();
        let dec = sup.on_exit("p", ExitReason::Normal, now()).unwrap();
        assert_eq!(dec, RestartDecision::Stop);

        sup.register("q", RestartPolicy::Transient).unwrap();
        sup.mark_active("q", now()).unwrap();
        let dec = sup.on_exit("q", ExitReason::Abnormal, now()).unwrap();
        assert_eq!(dec, RestartDecision::Restart);
    }

    #[test]
    fn permanent_policy_restarts_on_both_normal_and_abnormal() {
        let sup = Supervisor::new();
        sup.register("p", RestartPolicy::Permanent).unwrap();
        sup.mark_active("p", now()).unwrap();
        let dec = sup.on_exit("p", ExitReason::Normal, now()).unwrap();
        assert_eq!(dec, RestartDecision::Restart);
        let dec = sup.on_exit("p", ExitReason::Abnormal, now()).unwrap();
        assert_eq!(dec, RestartDecision::Restart);
    }

    #[test]
    fn budget_exhaustion_disables_plugin() {
        let sup = Supervisor::new();
        sup.register("p", RestartPolicy::Permanent).unwrap();
        sup.mark_active("p", now()).unwrap();
        let t = now();
        // 1st abnormal: restart
        assert_eq!(
            sup.on_exit("p", ExitReason::Abnormal, t).unwrap(),
            RestartDecision::Restart
        );
        // 2nd abnormal: restart
        assert_eq!(
            sup.on_exit("p", ExitReason::Abnormal, t).unwrap(),
            RestartDecision::Restart
        );
        // 3rd abnormal: disabled (budget = 3 in 60s)
        assert_eq!(
            sup.on_exit("p", ExitReason::Abnormal, t).unwrap(),
            RestartDecision::Disable {
                reason: DisableReason::CrashBudgetExhausted
            }
        );
        assert_eq!(sup.status("p").unwrap(), Some(PluginStatus::Disabled));
    }

    #[test]
    fn restart_count_increments_on_restart_decisions() {
        let sup = Supervisor::new();
        sup.register("p", RestartPolicy::Transient).unwrap();
        sup.mark_active("p", now()).unwrap();
        sup.on_exit("p", ExitReason::Abnormal, now()).unwrap();
        sup.on_exit("p", ExitReason::Abnormal, now()).unwrap();
        let state = sup.state("p").unwrap().unwrap();
        assert_eq!(state.restart_count, 2);
    }

    #[test]
    fn normal_exit_does_not_count_against_budget() {
        let sup = Supervisor::new();
        sup.register("p", RestartPolicy::Permanent).unwrap();
        sup.mark_active("p", now()).unwrap();
        let t = now();
        // 10 normal cycles — Permanent restarts each time, no crash
        // counted, plugin stays Active-eligible.
        for _ in 0..10 {
            let dec = sup.on_exit("p", ExitReason::Normal, t).unwrap();
            assert_eq!(dec, RestartDecision::Restart);
        }
        let state = sup.state("p").unwrap().unwrap();
        assert_eq!(state.crash_budget.count_in_window(t), 0);
    }

    #[test]
    fn re_enable_resets_budget_and_status() {
        let sup = Supervisor::new();
        sup.register("p", RestartPolicy::Permanent).unwrap();
        sup.mark_active("p", now()).unwrap();
        let t = now();
        for _ in 0..3 {
            let _ = sup.on_exit("p", ExitReason::Abnormal, t).unwrap();
        }
        assert_eq!(sup.status("p").unwrap(), Some(PluginStatus::Disabled));
        sup.re_enable("p").unwrap();
        let state = sup.state("p").unwrap().unwrap();
        assert_eq!(state.status, PluginStatus::Loaded);
        assert_eq!(state.restart_count, 0);
        assert_eq!(state.crash_budget.count_in_window(t), 0);
    }

    #[test]
    fn on_exit_unknown_plugin_returns_not_found() {
        let sup = Supervisor::new();
        let err = sup.on_exit("unknown", ExitReason::Abnormal, now()).err();
        assert!(matches!(err, Some(PluginError::PluginNotFound(_))));
    }

    #[test]
    fn list_returns_every_registered_plugin() {
        let sup = Supervisor::new();
        sup.register("a", RestartPolicy::Permanent).unwrap();
        sup.register("b", RestartPolicy::Transient).unwrap();
        sup.register("c", RestartPolicy::Temporary).unwrap();
        let list = sup.list().unwrap();
        assert_eq!(list.len(), 3);
        for id in ["a", "b", "c"] {
            assert!(list.iter().any(|(name, _)| name == id));
        }
    }

    #[test]
    fn crash_budget_prune_drops_old_entries() {
        let mut b = CrashBudget::new(3, Duration::from_millis(50));
        let t0 = Instant::now();
        b.record_crash(t0);
        let t1 = t0 + Duration::from_millis(200);
        assert_eq!(b.count_in_window(t1), 0);
        // Recording another crash at t1 should leave count = 1.
        b.record_crash(t1);
        assert_eq!(b.count_in_window(t1), 1);
    }

    #[test]
    fn crash_budget_reset_clears_recorded_crashes() {
        let mut b = CrashBudget::default_budget();
        let t = Instant::now();
        b.record_crash(t);
        b.record_crash(t);
        assert_eq!(b.count_in_window(t), 2);
        b.reset();
        assert_eq!(b.count_in_window(t), 0);
    }

    #[test]
    fn re_enabling_a_never_disabled_plugin_is_a_noop() {
        let sup = Supervisor::new();
        sup.register("p", RestartPolicy::Transient).unwrap();
        sup.re_enable("p").unwrap();
        let state = sup.state("p").unwrap().unwrap();
        assert_eq!(state.status, PluginStatus::Loaded);
    }
}
