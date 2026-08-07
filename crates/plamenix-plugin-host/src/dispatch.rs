//! Event dispatch — the call from the bus into a live plugin.
//!
//! Three pieces already existed and never met. [`crate::event_bus`]
//! knows who subscribed to a topic. [`crate::instance::InstanceRegistry`]
//! holds the live store each of those plugins is running in. The WIT
//! contract exports `handle-event`. Nothing joined them, so emitting an
//! event produced a list of interested plugins that nobody ever called.
//!
//! This module is that join, and it is deliberately the only place that
//! calls a plugin on the host's behalf, because the rules below have to
//! hold for every such call and are easy to forget one at a time:
//!
//! * **One plugin's failure is its own.** A trap, a timeout, or a
//!   missing instance is recorded and dispatch continues to the rest.
//!   Events are broadcast; letting the first bad subscriber cancel the
//!   others would make one plugin's bug look like the host's.
//! * **Every call is bounded.** The epoch deadline is set immediately
//!   before the call rather than at activation, because a deadline is
//!   consumed once it passes — a store that survived one slow call
//!   would otherwise start its next call already expired.
//! * **Every call takes a permit.** The in-flight limiter caps how much
//!   work can be queued into one plugin, so a slow subscriber applies
//!   backpressure instead of accumulating an unbounded backlog.

use std::time::Instant;

use crate::concurrency::InFlightAcquireError;
use crate::epoch::CallClass;
use crate::event_bus::EventBus;
use crate::instance::InstanceRegistry;
use crate::supervisor::{ExitReason, RestartDecision, Supervisor};
use crate::trap::{CallFailure, FailureKind};

/// What happened when the host called one plugin's `handle-event`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// The plugin handled the event and returned.
    Delivered,
    /// The plugin subscribed but has no live instance — it is UI-only,
    /// or it failed to activate. Not an error: subscriptions are
    /// recorded from the manifest at parse time, before anything is
    /// instantiated.
    NotInstantiated,
    /// The call trapped, exceeded its deadline, or the plugin panicked.
    /// Carries the classified reason — see [`crate::trap::CallFailure`]
    /// for why the reason has to be extracted rather than stringified.
    Failed(CallFailure),
    /// The plugin is shutting down and is no longer accepting calls.
    Closed,
}

/// One subscriber's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    /// Plugin the event was dispatched to.
    pub plugin_id: String,
    /// What came of it.
    pub outcome: DispatchOutcome,
}

/// Emits `topic` and calls `handle-event` on every subscriber that has
/// a live instance.
///
/// Returns one [`Delivery`] per matched subscriber, in match order. The
/// return value is the supervisor's input: a `Failed` entry is what
/// tells it a plugin misbehaved on this event.
///
/// Never returns an error. A broadcast that aborts partway is worse
/// than one that reports per-subscriber outcomes, since the caller
/// cannot tell which subscribers ran.
pub async fn dispatch_event(
    bus: &EventBus,
    registry: &InstanceRegistry,
    topic: &str,
    payload: &str,
) -> Vec<Delivery> {
    let matched = bus.emit(topic);
    let mut deliveries = Vec::with_capacity(matched.len());

    for subscriber in matched {
        let plugin_id = subscriber.plugin_id;
        let outcome = dispatch_one(registry, &plugin_id, topic, payload).await;
        if let DispatchOutcome::Failed(ref failure) = outcome {
            tracing::warn!(
                plugin = %plugin_id,
                topic,
                kind = %failure.kind,
                message = %failure.message,
                "plugin failed to handle event",
            );
        }
        deliveries.push(Delivery { plugin_id, outcome });
    }

    deliveries
}

async fn dispatch_one(
    registry: &InstanceRegistry,
    plugin_id: &str,
    topic: &str,
    payload: &str,
) -> DispatchOutcome {
    let instance = match registry.get(plugin_id) {
        Ok(Some(instance)) => instance,
        // A poisoned registry lock is reported against the plugin
        // rather than propagated: the other subscribers are unaffected
        // and should still receive the event.
        Ok(None) => return DispatchOutcome::NotInstantiated,
        // A poisoned lock is the host's problem, not the guest's, so it
        // is not reported as a trap.
        Err(err) => {
            return DispatchOutcome::Failed(CallFailure {
                kind: FailureKind::Host,
                message: err.to_string(),
            });
        }
    };

    let _permit = match instance.acquire_call_permit().await {
        Ok(permit) => permit,
        Err(InFlightAcquireError::Closed) => return DispatchOutcome::Closed,
    };

    let mut store = instance.lock_store().await;
    // Set per call, not once at activation: a deadline is spent when it
    // passes, so a store that has already been preempted would enter
    // its next call with no budget at all.
    store.set_epoch_deadline(CallClass::Interactive.deadline_ticks());

    match instance
        .bindings()
        .plamenix_plugin_plugin()
        .call_handle_event(&mut *store, topic, payload)
        .await
    {
        Ok(()) => DispatchOutcome::Delivered,
        Err(err) => DispatchOutcome::Failed(CallFailure::from_error(&err)),
    }
}

/// A [`Delivery`] plus, when the plugin failed, what the supervisor
/// decided to do about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupervisedDelivery {
    /// Plugin the event was dispatched to.
    pub plugin_id: String,
    /// What came of the call.
    pub outcome: DispatchOutcome,
    /// The supervisor's verdict. `None` unless the call failed —
    /// a delivered event is not an exit and must not consume crash
    /// budget.
    pub decision: Option<RestartDecision>,
}

/// Dispatches `topic` and reports every failure to `supervisor`.
///
/// The supervisor's whole purpose is to notice a plugin misbehaving,
/// and a plugin that traps on every event it receives is the clearest
/// case there is. Without this the crash budget only ever saw
/// activation failures, so a plugin could trap on a thousand events
/// and stay `Active`.
///
/// A failure the supervisor does not recognise — an unregistered
/// plugin id — is logged and left without a decision rather than
/// failing the dispatch. Event delivery is not the place to discover
/// registration bookkeeping problems.
pub async fn dispatch_event_supervised(
    bus: &EventBus,
    registry: &InstanceRegistry,
    supervisor: &Supervisor,
    topic: &str,
    payload: &str,
) -> Vec<SupervisedDelivery> {
    let deliveries = dispatch_event(bus, registry, topic, payload).await;
    let now = Instant::now();

    deliveries
        .into_iter()
        .map(|delivery| {
            let decision = match delivery.outcome {
                // A trap or a missed deadline is an abnormal exit: the
                // plugin did not choose to stop.
                DispatchOutcome::Failed(_) => {
                    match supervisor.on_exit(&delivery.plugin_id, ExitReason::Abnormal, now) {
                        Ok(decision) => Some(decision),
                        Err(err) => {
                            tracing::warn!(
                                plugin = %delivery.plugin_id,
                                ?err,
                                "plugin failed an event but is not registered with the supervisor",
                            );
                            None
                        }
                    }
                }
                _ => None,
            };
            SupervisedDelivery {
                plugin_id: delivery.plugin_id,
                outcome: delivery.outcome,
                decision,
            }
        })
        .collect()
}
