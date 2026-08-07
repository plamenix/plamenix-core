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

use std::collections::VecDeque;
use std::time::Instant;

use crate::concurrency::InFlightAcquireError;
use crate::event_bus::EventBus;
use crate::host_impl::PendingEmit;
use crate::instance::InstanceRegistry;
use crate::supervisor::{ExitReason, RestartDecision, Supervisor};
use crate::trap::{CallFailure, FailureKind};

/// How many times an emitted event may itself be re-dispatched.
///
/// A plugin that emits the topic it subscribes to is a cycle, and the
/// cap is what makes it terminate rather than livelock. Four hops is
/// deep enough for a plugin to react to another plugin's reaction and
/// shallow enough that nobody waits on it.
pub const MAX_EMIT_DEPTH: u8 = 4;

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
    let mut queue: VecDeque<(String, String, u8)> =
        VecDeque::from([(topic.to_owned(), payload.to_owned(), 0)]);
    let mut deliveries = Vec::new();

    // Breadth-first over the cascade, iterative rather than recursive:
    // an async fn that calls itself needs boxing, and the depth here is
    // driven by what plugins do.
    while let Some((topic, payload, depth)) = queue.pop_front() {
        for subscriber in bus.emit(&topic) {
            let plugin_id = subscriber.plugin_id;
            let (outcome, emitted) =
                dispatch_one(registry, &plugin_id, &topic, &payload, depth).await;
            if let DispatchOutcome::Failed(ref failure) = outcome {
                tracing::warn!(
                    plugin = %plugin_id,
                    topic = %topic,
                    kind = %failure.kind,
                    message = %failure.message,
                    "plugin failed to handle event",
                );
            }
            deliveries.push(Delivery { plugin_id, outcome });

            if depth >= MAX_EMIT_DEPTH {
                if !emitted.is_empty() {
                    tracing::warn!(
                        topic = %topic,
                        depth,
                        dropped = emitted.len(),
                        "cascade hit the depth cap; dropping what it emitted",
                    );
                }
                continue;
            }
            queue.extend(
                emitted
                    .into_iter()
                    .map(|emit| (emit.topic, emit.payload, depth + 1)),
            );
        }
    }

    deliveries
}

/// Calls one plugin, and returns whatever it asked to emit.
///
/// The emits come back rather than being dispatched here because this
/// function holds the plugin's store for the whole call. Delivering
/// inline would re-lock a store that is already locked — for a plugin
/// subscribed to its own topic, its own — and `tokio::sync::Mutex` is
/// not reentrant, so the task would block forever with no epoch
/// deadline to break it, because no wasm would be executing.
async fn dispatch_one(
    registry: &InstanceRegistry,
    plugin_id: &str,
    topic: &str,
    payload: &str,
    depth: u8,
) -> (DispatchOutcome, Vec<PendingEmit>) {
    let instance = match registry.get(plugin_id) {
        Ok(Some(instance)) => instance,
        // Subscriptions are recorded from the manifest before anything
        // is instantiated, so a UI-only plugin legitimately appears
        // here with no store to call.
        Ok(None) => return (DispatchOutcome::NotInstantiated, Vec::new()),
        // A poisoned lock is the host's problem, not the guest's, so it
        // is not reported as a trap.
        Err(err) => {
            return (
                DispatchOutcome::Failed(CallFailure {
                    kind: FailureKind::Host,
                    message: err.to_string(),
                }),
                Vec::new(),
            );
        }
    };

    let _permit = match instance.acquire_call_permit().await {
        Ok(permit) => permit,
        Err(InFlightAcquireError::Closed) => return (DispatchOutcome::Closed, Vec::new()),
    };

    let mut store = instance.lock_store().await;
    {
        let state = store.data_mut();
        state.call_depth = depth;
        state.outbox.clear();
        state.host_import_ticks = 0;
        // Once per call rather than once per import: grants live behind
        // a lock in the shell, and a call that saw a permission appear
        // halfway through would be harder to reason about.
        state.refresh_grants();
    }
    // Set per call, not once at activation: a deadline is spent when it
    // passes, so a store that has already been preempted would enter
    // its next call with no budget at all.
    //
    // The class depends on what the plugin declared — see
    // `HostState::dispatch_call_class`. A plugin that can query the
    // database cannot be held to the interactive budget without being
    // disabled for our own latency.
    let class = store.data().dispatch_call_class();
    store.set_epoch_deadline(class.deadline_ticks());

    let result = instance
        .bindings()
        .plamenix_plugin_plugin()
        .call_handle_event(&mut *store, topic, payload)
        .await;

    let emitted = std::mem::take(&mut store.data_mut().outbox);
    // Released before the caller can re-dispatch anything this plugin
    // emitted. The ordering is the whole design.
    drop(store);

    let outcome = match result {
        Ok(()) => DispatchOutcome::Delivered,
        Err(err) => DispatchOutcome::Failed(CallFailure::from_error(&err)),
    };
    (outcome, emitted)
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
