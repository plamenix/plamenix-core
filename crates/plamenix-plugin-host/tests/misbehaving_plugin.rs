//! Fires the host's guarantees with a plugin that actually misbehaves.
//!
//! Everything here was previously proven by construction: preemption by
//! setting an epoch deadline by hand, the crash budget by calling
//! `on_exit` directly, the memory cap by checking arithmetic. None of
//! that involves a plugin misbehaving, and a guarantee that has never
//! been fired is one nobody has watched work.
//!
//! The `misbehaving-plugin` fixture picks its behaviour from the event
//! topic — see `examples/misbehaving-plugin/src/lib.rs`.
//!
//! **If a test here hangs rather than fails, that is the finding.** The
//! looping case has no way to end except preemption, so a hang means
//! the epoch deadline never fired.
//!
//! ## Why the runaway tests need a multi-threaded runtime
//!
//! Writing them exposed a real constraint on the guarantee. The epoch
//! ticker is a Tokio task, and a plugin spinning inside wasm holds the
//! thread it is running on without yielding. On a current-thread
//! runtime — which is what `#[tokio::test]` gives you by default — the
//! ticker is never scheduled, the epoch never advances, and the call
//! runs forever: the test pegs a core rather than failing.
//!
//! So preemption depends on the ticker living on a different thread
//! from the call it is policing. Both shells satisfy that (Tauri and
//! napi-rs both run multi-threaded runtimes), but it is a property of
//! the host embedding rather than of this crate, and nothing else
//! states it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

use plamenix_plugin_host::{
    DisableReason, DispatchOutcome, EpochTicker, EventBus, FailureKind, HostState,
    InstanceRegistry, PluginHost, PluginStatus, RestartDecision, RestartPolicy, Supervisor,
    activate_into_registry, dispatch_event, dispatch_event_supervised, load,
};
use semver::Version;

const MISBEHAVING_WASM: &[u8] = include_bytes!("fixtures/misbehaving-plugin.wasm");
const PLUGIN_ID: &str = "dev.plamenix.misbehaving";

fn stage(dir: &std::path::Path, limits: Option<&str>) {
    std::fs::write(dir.join("plugin.wasm"), MISBEHAVING_WASM).unwrap();
    let limits_table = limits.unwrap_or("");
    std::fs::write(
        dir.join("manifest.toml"),
        format!(
            r#"
[plugin]
id = "dev.plamenix.misbehaving"
name = "Misbehaving Fixture"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"
{limits_table}
"#
        ),
    )
    .unwrap();
}

async fn activate(
    host: &PluginHost,
    registry: &InstanceRegistry,
    dir: &std::path::Path,
    limits: Option<&str>,
) {
    stage(dir, limits);
    let version = Version::parse("1.0.0-beta").expect("version");
    let staged = load(host, &version, dir).expect("load");
    activate_into_registry(
        host,
        HostState::new(&staged.manifest.plugin.id, "1.0.0-beta"),
        &staged,
        registry,
    )
    .await
    .expect("activate");
}

fn subscribed_bus() -> EventBus {
    let bus = EventBus::new();
    bus.subscribe(PLUGIN_ID, "misbehave/**").expect("subscribe");
    bus.subscribe(PLUGIN_ID, "behave/**").expect("subscribe");
    bus
}

#[tokio::test]
async fn a_trapping_plugin_is_reported_not_propagated() {
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    activate(&host, &registry, dir.path(), None).await;

    let deliveries = dispatch_event(&subscribed_bus(), &registry, "misbehave/trap", "{}").await;

    let DispatchOutcome::Failed(ref failure) = deliveries[0].outcome else {
        panic!(
            "a trap must surface as a failed delivery, got {:?}",
            deliveries[0].outcome,
        );
    };
    assert_eq!(failure.kind, FailureKind::Trapped);
    // The whole point of classifying: the reason wasmtime reports lives
    // in the error's *source*, so a host that stringified the outermost
    // layer would record only "error while executing at wasm backtrace".
    assert!(
        failure.message.contains("unreachable"),
        "the trap's reason did not survive into the message: {}",
        failure.message,
    );
}

#[tokio::test]
async fn a_trap_does_not_take_the_instance_down_with_it() {
    // wasmtime poisons the store on a trap, so the question is whether
    // the host survives to keep serving other work rather than whether
    // this plugin recovers.
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    activate(&host, &registry, dir.path(), None).await;
    let bus = subscribed_bus();

    let _ = dispatch_event(&bus, &registry, "misbehave/trap", "{}").await;

    // The registry still holds it, and dispatch still answers rather
    // than panicking or hanging.
    assert!(registry.get(PLUGIN_ID).expect("lock").is_some());
    let after = dispatch_event(&bus, &registry, "behave/ok", "{}").await;
    assert_eq!(after.len(), 1, "dispatch stopped working after a trap");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_looping_plugin_is_preempted_rather_than_hanging_the_host() {
    // The fixture spins forever. Nothing but the epoch deadline can end
    // this call, so a hang here means preemption is not working.
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let ticker = EpochTicker::spawn(host.engine().clone());
    activate(&host, &registry, dir.path(), None).await;

    let started = Instant::now();
    let deliveries = tokio::time::timeout(
        Duration::from_secs(20),
        dispatch_event(&subscribed_bus(), &registry, "misbehave/loop", "{}"),
    )
    .await
    .expect("the call never returned — the epoch deadline did not fire");

    // Naming the reason matters here. `Failed(_)` alone would also be
    // satisfied by a trap for some unrelated reason, which would let
    // this pass while preemption stayed broken. This is also the only
    // thing holding up the deadline-to-`Trap::Interrupt` mapping in
    // `src/trap.rs` — wasmtime does not document it.
    let DispatchOutcome::Failed(ref failure) = deliveries[0].outcome else {
        panic!(
            "a preempted call must surface as failed, got {:?}",
            deliveries[0].outcome,
        );
    };
    assert_eq!(
        failure.kind,
        FailureKind::Deadline,
        "the call ended for some reason other than preemption: {}",
        failure.message,
    );
    // Dispatch uses the interactive class (100ms). Generous upper bound
    // so a loaded machine does not make this flaky, but far below the
    // 20s timeout that would mean no preemption at all.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "preemption took {:?}, which suggests it is not the thing that ended the call",
        started.elapsed(),
    );

    ticker.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_memory_hungry_plugin_hits_its_cap_instead_of_the_machine() {
    // 16 MiB, well under the 64 MiB default, so the cap is unambiguously
    // what stops it rather than the host running out of anything.
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    let ticker = EpochTicker::spawn(host.engine().clone());
    activate(
        &host,
        &registry,
        dir.path(),
        Some("\n[runtime.limits]\nmax_memory_mib = 16\n"),
    )
    .await;

    let deliveries = tokio::time::timeout(
        Duration::from_secs(30),
        dispatch_event(&subscribed_bus(), &registry, "misbehave/grow", "{}"),
    )
    .await
    .expect("the allocation loop never ended — neither the memory cap nor the deadline fired");

    let DispatchOutcome::Failed(ref failure) = deliveries[0].outcome else {
        panic!(
            "hitting the memory cap must surface as failed, got {:?}",
            deliveries[0].outcome,
        );
    };
    // The ticker is running here too, so preemption is a competing
    // explanation for the call ending. Ruling it out is what makes this
    // a test of the memory cap rather than of the deadline again.
    //
    // The cap surfaces as a trap rather than as its own kind: the
    // guest's allocator asks for memory, the host's ResourceLimiter
    // refuses, and the guest panics on the failed allocation. The host
    // sees the panic, not the refusal.
    assert_eq!(
        failure.kind,
        FailureKind::Trapped,
        "the deadline ended this call before the memory cap could: {}",
        failure.message,
    );

    ticker.stop().await;
}

#[tokio::test]
async fn repeated_traps_disable_the_plugin_through_the_real_path() {
    // The supervisor test elsewhere drives `on_exit` directly. This one
    // reaches it the way production does: a plugin traps, dispatch
    // reports it, the budget is charged.
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    activate(&host, &registry, dir.path(), None).await;

    let supervisor = Supervisor::new();
    supervisor
        .register(PLUGIN_ID, RestartPolicy::Transient)
        .expect("register");
    supervisor
        .mark_active(PLUGIN_ID, Instant::now())
        .expect("mark active");
    let bus = subscribed_bus();

    let mut last = None;
    // Default budget is 3 crashes in 60s.
    for _ in 0..4 {
        let out =
            dispatch_event_supervised(&bus, &registry, &supervisor, "misbehave/trap", "{}").await;
        last = out[0].decision;
    }

    assert_eq!(
        last,
        Some(RestartDecision::Disable {
            reason: DisableReason::CrashBudgetExhausted
        }),
        "a plugin trapping on every event must end up disabled",
    );
    assert_eq!(
        supervisor.status(PLUGIN_ID).expect("status"),
        Some(PluginStatus::Disabled),
    );
}

#[tokio::test]
async fn a_well_behaved_event_still_succeeds_on_the_same_fixture() {
    // The control. Without it, a fixture that failed to load would make
    // every assertion above pass for the wrong reason.
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");
    activate(&host, &registry, dir.path(), None).await;

    let deliveries = dispatch_event(&subscribed_bus(), &registry, "behave/ok", "{}").await;
    assert_eq!(deliveries[0].outcome, DispatchOutcome::Delivered);
}
