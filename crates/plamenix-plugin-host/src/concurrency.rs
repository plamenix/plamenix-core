//! Per-plugin concurrent-call cap (I8.8).
//!
//! Why a cap exists — a runaway dispatcher (event fan-out, command
//! palette repeat, accidental loop) could queue thousands of
//! host→plugin calls. Each call holds a `tokio::sync::MutexGuard` on
//! the plugin's wasmtime store + a stack frame in the host. Without
//! a cap, a small subset of plugins can pin runtime resources +
//! starve sibling plugins of progress.
//!
//! [`InFlightLimiter`] wraps a [`tokio::sync::Semaphore`] with a
//! per-plugin capacity (default 4). Callers `acquire().await` a
//! [`CallPermit`] before invoking the plugin; the permit drops when
//! the call resolves (success, error, or trap), releasing the slot
//! back to the semaphore.
//!
//! ## Why a permit type instead of bare `OwnedSemaphorePermit`
//!
//! [`CallPermit`] is a thin newtype wrapper around the permit. The
//! wrapping serves three purposes today and a fourth in M2:
//!
//! 1. Callers can search the codebase for "call permit" to find the
//!    invariants ("don't drop before the await resolves").
//! 2. The host can attach diagnostic context (plugin id, acquired-at
//!    instant, call class) by extending the struct without touching
//!    call sites.
//! 3. It makes the permit move-only by default (the inner type is
//!    not `Copy`) — accidentally storing two copies in a future
//!    won't compile.
//! 4. M2: surfacing wait-time histograms requires recording when the
//!    permit was requested + when it was granted. That data lands
//!    in [`CallPermit`] without a public API break.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Default concurrent in-flight call cap per plugin.
///
/// Picked at 4 so an interactive plugin (e.g. inline-edit hook) can
/// run + a sibling event-handler dispatch can proceed without head-
/// of-line blocking, but a buggy fan-out can't queue arbitrary work.
pub const MAX_IN_FLIGHT_DEFAULT: usize = 4;

/// Per-plugin concurrent-call limiter.
///
/// Cheap to clone — clones share the inner `Arc<Semaphore>`. Each
/// plugin instance owns one [`InFlightLimiter`]; the activator
/// constructs it once at activation time + hands it to the
/// [`crate::instance::PluginInstance`] for the call sites to
/// `acquire()` against.
#[derive(Clone, Debug)]
pub struct InFlightLimiter {
    inner: Arc<Semaphore>,
    capacity: usize,
}

impl InFlightLimiter {
    /// Builds a limiter with the supplied capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Semaphore::new(capacity)),
            capacity,
        }
    }

    /// Returns a limiter at the [`MAX_IN_FLIGHT_DEFAULT`] cap.
    #[must_use]
    pub fn with_default_capacity() -> Self {
        Self::new(MAX_IN_FLIGHT_DEFAULT)
    }

    /// Returns the configured capacity (the number of permits the
    /// limiter was constructed with). Constant for the lifetime of
    /// the limiter — Tokio's `Semaphore` itself doesn't expose
    /// capacity directly.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of permits currently available (not in flight).
    ///
    /// `capacity - in_flight()` always equals this; provided as a
    /// convenience for tests + the Permissions panel.
    #[must_use]
    pub fn available_permits(&self) -> usize {
        self.inner.available_permits()
    }

    /// Number of in-flight calls (permits currently held).
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.capacity.saturating_sub(self.inner.available_permits())
    }

    /// Acquires a [`CallPermit`]. Awaits until a slot is free.
    ///
    /// # Errors
    ///
    /// Returns a [`InFlightAcquireError::Closed`] when the inner
    /// semaphore has been closed (typically because the plugin was
    /// uninstalled while a call was pending). Callers MUST treat the
    /// closed signal as a clean cancellation and propagate it back to
    /// the dispatcher.
    pub async fn acquire(&self) -> Result<CallPermit, InFlightAcquireError> {
        let permit = Arc::clone(&self.inner)
            .acquire_owned()
            .await
            .map_err(|_| InFlightAcquireError::Closed)?;
        Ok(CallPermit { _permit: permit })
    }

    /// Tries to acquire a permit without awaiting. Returns `None`
    /// when the cap is currently saturated.
    pub fn try_acquire(&self) -> Result<Option<CallPermit>, InFlightAcquireError> {
        use tokio::sync::TryAcquireError;
        match Arc::clone(&self.inner).try_acquire_owned() {
            Ok(permit) => Ok(Some(CallPermit { _permit: permit })),
            Err(TryAcquireError::NoPermits) => Ok(None),
            Err(TryAcquireError::Closed) => Err(InFlightAcquireError::Closed),
        }
    }

    /// Closes the limiter. In-flight calls retain their permits +
    /// continue running; future `acquire` calls return
    /// [`InFlightAcquireError::Closed`]. Used at plugin teardown so
    /// the dispatcher can no longer schedule new work into a plugin
    /// being uninstalled.
    pub fn close(&self) {
        self.inner.close();
    }
}

/// Permit held for the duration of a single in-flight plugin call.
/// Dropping it returns the slot to the limiter.
#[derive(Debug)]
pub struct CallPermit {
    _permit: OwnedSemaphorePermit,
}

/// Errors from acquiring an in-flight permit.
#[derive(Debug, thiserror::Error)]
pub enum InFlightAcquireError {
    /// The limiter was closed before the call could acquire a permit.
    #[error("in-flight limiter closed (plugin being uninstalled or torn down)")]
    Closed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capacity_is_four() {
        let l = InFlightLimiter::with_default_capacity();
        assert_eq!(l.capacity(), 4);
        assert_eq!(l.available_permits(), 4);
        assert_eq!(l.in_flight(), 0);
    }

    #[test]
    fn custom_capacity_is_honoured() {
        let l = InFlightLimiter::new(2);
        assert_eq!(l.capacity(), 2);
        assert_eq!(l.available_permits(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acquire_consumes_a_permit() {
        let l = InFlightLimiter::new(2);
        let p = l.acquire().await.unwrap();
        assert_eq!(l.available_permits(), 1);
        assert_eq!(l.in_flight(), 1);
        drop(p);
        assert_eq!(l.available_permits(), 2);
        assert_eq!(l.in_flight(), 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn try_acquire_returns_none_when_saturated() {
        let l = InFlightLimiter::new(1);
        let _hold = l.acquire().await.unwrap();
        let attempt = l.try_acquire().unwrap();
        assert!(attempt.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn try_acquire_succeeds_when_slot_free() {
        let l = InFlightLimiter::new(1);
        let p = l.try_acquire().unwrap();
        assert!(p.is_some());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acquire_blocks_until_permit_returned() {
        let l = InFlightLimiter::new(1);
        let held = l.acquire().await.unwrap();
        let l2 = l.clone();
        let task = tokio::spawn(async move { l2.acquire().await });
        // Verify the task is blocked: small sleep, then check it
        // hasn't completed yet.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!task.is_finished());
        drop(held);
        let permit = task.await.unwrap();
        assert!(permit.is_ok());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn close_makes_future_acquires_fail() {
        let l = InFlightLimiter::new(2);
        l.close();
        let err = l.acquire().await;
        assert!(matches!(err, Err(InFlightAcquireError::Closed)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn close_does_not_revoke_held_permits() {
        let l = InFlightLimiter::new(2);
        let held = l.acquire().await.unwrap();
        l.close();
        // Holder still owns its permit; drop completes cleanly.
        drop(held);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clone_shares_inner_semaphore() {
        let a = InFlightLimiter::new(2);
        let b = a.clone();
        let p = a.acquire().await.unwrap();
        // Clone observes the same in-flight count.
        assert_eq!(b.in_flight(), 1);
        drop(p);
        assert_eq!(b.in_flight(), 0);
    }
}
