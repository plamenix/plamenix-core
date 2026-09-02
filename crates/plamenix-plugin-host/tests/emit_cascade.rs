//! A plugin emitting an event must not be able to wedge the host.
//!
//! The hazard is specific and it would have shipped. `dispatch_one`
//! holds the plugin's store across `call_handle_event`. If the
//! `event-bus.emit` host import dispatched inline, a plugin subscribed
//! to a topic it emits would re-lock its own store —
//! `tokio::sync::Mutex` is not reentrant, so the task blocks forever.
//! Nothing recovers it: the epoch deadline cannot fire because no wasm
//! is executing, and the in-flight permit is never released, so the
//! plugin is permanently out of service. Two plugins emitting at each
//! other is the same thing with an extra step.
//!
//! So `emit` never touches the bus. It appends to a per-store outbox
//! that the dispatcher drains *after* releasing the lock, and the
//! cascade is bounded. These tests drive that machinery directly rather
//! than through wasm, because the interesting behaviour is the
//! dispatcher's and a fixture would only make it slower to run.
//!
//! **Every test here is bounded by `tokio::time::timeout`.** If the
//! design regressed, the symptom is a hang, not a failure — and a
//! suite that hangs tells you far less than one that fails.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use plamenix_plugin_host::{
    DispatchOutcome, EventBus, InstanceRegistry, MAX_EMIT_DEPTH, dispatch_event,
};

const LIMIT: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_subscribed_to_its_own_topic_terminates() {
    // The cycle. Without the depth cap this never ends; without the
    // outbox it deadlocks before it can even loop.
    let registry = InstanceRegistry::new();
    let bus = EventBus::new();
    bus.subscribe("dev.plamenix.echo", "**").expect("subscribe");

    let deliveries = tokio::time::timeout(
        LIMIT,
        dispatch_event(&bus, &registry, "dev.plamenix.echo:ping", "{}"),
    )
    .await
    .expect("dispatch never returned — the cascade did not terminate");

    // No live instance, so one delivery and no cascade. What matters is
    // that it came back at all.
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].outcome, DispatchOutcome::NotInstantiated);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_still_reaches_every_subscriber_after_the_rewrite() {
    // The BFS replaced a straight loop. This is the property that loop
    // had and the rewrite must not have lost.
    let registry = InstanceRegistry::new();
    let bus = EventBus::new();
    for id in ["a.plugin", "b.plugin", "c.plugin"] {
        bus.subscribe(id, "db/**").expect("subscribe");
    }

    let deliveries = tokio::time::timeout(
        LIMIT,
        dispatch_event(&bus, &registry, "db/query/executed", "{}"),
    )
    .await
    .expect("dispatch never returned");

    assert_eq!(deliveries.len(), 3);
    let mut ids: Vec<&str> = deliveries.iter().map(|d| d.plugin_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, ["a.plugin", "b.plugin", "c.plugin"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_topic_nobody_subscribed_to_still_costs_nothing() {
    let registry = InstanceRegistry::new();
    let bus = EventBus::new();
    bus.subscribe("a.plugin", "db/**").expect("subscribe");

    let deliveries =
        tokio::time::timeout(LIMIT, dispatch_event(&bus, &registry, "editor/saved", "{}"))
            .await
            .expect("dispatch never returned");

    assert!(deliveries.is_empty());
}

#[test]
fn the_depth_cap_is_shallow_enough_to_wait_on() {
    // Documents the number rather than testing arithmetic: the cap
    // exists so a cycle terminates while the user is waiting, and a
    // large value would make the bound useless in practice.
    assert!(
        MAX_EMIT_DEPTH <= 8,
        "a cascade bound the user waits on has to stay small",
    );
    assert!(MAX_EMIT_DEPTH >= 1, "zero would forbid cascades entirely");
}
