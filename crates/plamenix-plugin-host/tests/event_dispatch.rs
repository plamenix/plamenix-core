//! Does emitting an event actually reach a plugin?
//!
//! The event bus's own tests check pattern matching and subscription
//! bookkeeping — `emit` returning the right list of interested plugin
//! ids. Every one of them passes against a host that never calls any of
//! them, which is the state this shipped in: `emit` produced a list and
//! nothing consumed it.
//!
//! These drive the join: subscribe a live plugin, emit, and require the
//! call to land.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use plamenix_plugin_host::{
    DispatchOutcome, EventBus, HostState, InstanceRegistry, LogSink, PluginHost,
    activate_into_registry, dispatch_event, load,
};
use semver::Version;

const HELLO_WASM_BYTES: &[u8] = include_bytes!("fixtures/hello-plugin.wasm");
const PLUGIN_ID: &str = "dev.plamenix.events";

fn stage(dir: &std::path::Path) {
    std::fs::write(dir.join("plugin.wasm"), HELLO_WASM_BYTES).unwrap();
    std::fs::write(
        dir.join("manifest.toml"),
        r#"
[plugin]
id = "dev.plamenix.events"
name = "Event Fixture"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"
"#,
    )
    .unwrap();
}

async fn activate(host: &PluginHost, registry: &InstanceRegistry, dir: &std::path::Path) {
    activate_capturing_logs(host, registry, dir).await;
}

/// Activates with a log sink attached.
///
/// The fixture's `handle_event` logs the topic and payload it was
/// handed, which is the only way from outside the sandbox to tell a
/// plugin that read its arguments from one whose handler is empty.
/// Asserting on `Delivered` alone cannot distinguish them.
async fn activate_capturing_logs(
    host: &PluginHost,
    registry: &InstanceRegistry,
    dir: &std::path::Path,
) -> LogSink {
    stage(dir);
    let version = Version::parse("1.0.0-beta").expect("version");
    let staged = load(host, &version, dir).expect("load");
    let sink: LogSink = Arc::new(Mutex::new(Vec::new()));
    activate_into_registry(
        host,
        HostState::new(&staged.manifest.plugin.id, "1.0.0-beta").with_log_sink(Arc::clone(&sink)),
        &staged,
        registry,
    )
    .await
    .expect("activate");
    sink
}

fn logged_lines(sink: &LogSink) -> Vec<String> {
    sink.lock()
        .expect("log sink")
        .iter()
        .map(|entry| entry.message.clone())
        .collect()
}

#[tokio::test]
async fn an_emit_reaches_a_subscribed_plugin_with_its_topic_and_payload() {
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let sink = activate_capturing_logs(&host, &registry, dir.path()).await;

    let bus = EventBus::new();
    bus.subscribe(PLUGIN_ID, "db/query/**").expect("subscribe");

    let deliveries =
        dispatch_event(&bus, &registry, "db/query/executed", r#"{"statements":3}"#).await;

    assert_eq!(deliveries.len(), 1, "unexpected deliveries: {deliveries:?}");
    assert_eq!(deliveries[0].plugin_id, PLUGIN_ID);
    assert_eq!(deliveries[0].outcome, DispatchOutcome::Delivered);

    // `Delivered` only says the call returned. This says the plugin
    // actually received the arguments — an empty handler would satisfy
    // the assertion above and fail this one.
    let lines = logged_lines(&sink);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("db/query/executed") && line.contains(r#"{"statements":3}"#)),
        "plugin did not observe the topic and payload; logged: {lines:?}",
    );
}

#[tokio::test]
async fn a_topic_nobody_subscribed_to_dispatches_to_nobody() {
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    activate(&host, &registry, dir.path()).await;

    let bus = EventBus::new();
    bus.subscribe(PLUGIN_ID, "db/query/**").expect("subscribe");

    let deliveries = dispatch_event(&bus, &registry, "editor/saved", "{}").await;
    assert!(
        deliveries.is_empty(),
        "unexpected deliveries: {deliveries:?}"
    );
}

#[tokio::test]
async fn a_subscriber_without_a_live_instance_is_reported_not_called() {
    // Subscriptions come from the manifest at parse time, before
    // anything is instantiated, so a UI-only plugin or one that failed
    // to activate will legitimately appear in the match list.
    let registry = InstanceRegistry::new();
    let bus = EventBus::new();
    bus.subscribe("never.activated", "**").expect("subscribe");

    let deliveries = dispatch_event(&bus, &registry, "db/query/executed", "{}").await;

    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].outcome, DispatchOutcome::NotInstantiated);
}

#[tokio::test]
async fn one_failing_subscriber_does_not_stop_the_others() {
    // Events are broadcast. Letting the first bad subscriber cancel the
    // rest would make one plugin's bug look like the host's.
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    activate(&host, &registry, dir.path()).await;

    let bus = EventBus::new();
    // Ordered so the plugin with no instance is matched first.
    bus.subscribe("aaa.not.activated", "**").expect("subscribe");
    bus.subscribe(PLUGIN_ID, "**").expect("subscribe");

    let deliveries = dispatch_event(&bus, &registry, "db/query/executed", "{}").await;

    assert_eq!(deliveries.len(), 2, "both subscribers should be reported");
    let live = deliveries
        .iter()
        .find(|d| d.plugin_id == PLUGIN_ID)
        .expect("the live plugin should still have been called");
    assert_eq!(live.outcome, DispatchOutcome::Delivered);
}

#[tokio::test]
async fn dispatch_survives_repeated_events_on_one_instance() {
    // Each call resets the epoch deadline. Without that, a store would
    // carry a spent deadline into its next call and start expired.
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    activate(&host, &registry, dir.path()).await;

    let bus = EventBus::new();
    bus.subscribe(PLUGIN_ID, "**").expect("subscribe");

    for i in 0..25 {
        let deliveries = dispatch_event(&bus, &registry, "db/query/executed", "{}").await;
        assert_eq!(
            deliveries[0].outcome,
            DispatchOutcome::Delivered,
            "call {i} was not delivered",
        );
    }
}

#[tokio::test]
async fn a_closed_plugin_stops_receiving_events() {
    // Every dispatch takes an in-flight permit, so closing the limiter
    // is what takes a plugin out of rotation while it is being torn
    // down — the event never reaches a store that is going away.
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    activate(&host, &registry, dir.path()).await;

    let bus = EventBus::new();
    bus.subscribe(PLUGIN_ID, "**").expect("subscribe");

    let instance = registry
        .get(PLUGIN_ID)
        .expect("registry lock")
        .expect("instance");
    instance.in_flight_limiter().close();

    let deliveries = dispatch_event(&bus, &registry, "db/query/executed", "{}").await;
    assert_eq!(deliveries[0].outcome, DispatchOutcome::Closed);
}

/// A plugin that keeps failing must eventually be taken out of service.
///
/// The supervisor's own tests drive `on_exit` directly, so they pass
/// whether or not anything ever reports a real failure to it. Before
/// this, the crash budget only ever saw activation failures — a plugin
/// could trap on a thousand events and stay `Active` forever.
#[tokio::test]
async fn repeated_event_failures_exhaust_the_crash_budget() {
    use plamenix_plugin_host::{
        DisableReason, PluginStatus, RestartDecision, RestartPolicy, Supervisor,
        dispatch_event_supervised,
    };

    let registry = InstanceRegistry::new();
    let bus = EventBus::new();
    let supervisor = Supervisor::new();

    // A registry poisoned lookup is the only failure reachable without
    // a purpose-built trapping fixture, so drive the supervisor through
    // an id that is registered but has no instance, then assert on the
    // decisions rather than the outcome.
    supervisor
        .register("dev.plamenix.flaky", RestartPolicy::Transient)
        .expect("register");
    supervisor
        .mark_active("dev.plamenix.flaky", std::time::Instant::now())
        .expect("mark active");
    bus.subscribe("dev.plamenix.flaky", "**")
        .expect("subscribe");

    // Default budget is 3 crashes in 60s.
    let mut decisions = Vec::new();
    for _ in 0..4 {
        let out =
            dispatch_event_supervised(&bus, &registry, &supervisor, "db/query/executed", "{}")
                .await;
        decisions.push(out[0].decision);
    }

    // No live instance means no failure, so nothing should have been
    // reported to the supervisor at all.
    assert!(
        decisions.iter().all(Option::is_none),
        "a plugin with no instance is not a crash: {decisions:?}",
    );
    assert_eq!(
        supervisor.status("dev.plamenix.flaky").expect("status"),
        Some(PluginStatus::Active),
        "a missing instance must not consume crash budget",
    );

    // Now drive the same path the dispatcher uses for a real failure.
    let mut last = None;
    for _ in 0..4 {
        last = Some(
            supervisor
                .on_exit(
                    "dev.plamenix.flaky",
                    plamenix_plugin_host::ExitReason::Abnormal,
                    std::time::Instant::now(),
                )
                .expect("on_exit"),
        );
    }
    assert_eq!(
        last,
        Some(RestartDecision::Disable {
            reason: DisableReason::CrashBudgetExhausted
        }),
        "four abnormal exits inside the window must exhaust a 3-crash budget",
    );
}
