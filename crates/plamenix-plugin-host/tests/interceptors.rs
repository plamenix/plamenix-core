//! Can a WASM plugin actually refuse or rewrite an operation?
//!
//! The TypeScript chain in `plamenix-ui/src/interceptors/` has been
//! tested for a while, and every one of those tests passes against a
//! build where no plugin can reach it — the WIT contract had no
//! interceptor export, so the plugin half of the feature did not exist.
//! That is the same shape as the event gap: a framework with no way in.
//!
//! These drive the whole path. A real component's `intercept` export is
//! called, its verdict comes back, and the chain semantics — priority
//! order, replace propagation, cancel short-circuit, fail-open — are
//! asserted against plugins that really behave that way rather than
//! against stubs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use plamenix_plugin_host::{
    ExtensionPoint, FailureKind, HostState, InstanceRegistry, InterceptorRegistration,
    InterceptorRegistry, PluginHost, PluginStatus, RestartPolicy, Supervisor, Verdict,
    activate_into_registry, intercept_one, intercept_one_supervised, load, run_chain,
};
use semver::Version;

const HELLO_WASM: &[u8] = include_bytes!("fixtures/hello-plugin.wasm");
const MISBEHAVING_WASM: &[u8] = include_bytes!("fixtures/misbehaving-plugin.wasm");

const HELLO_ID: &str = "dev.plamenix.hello";
const MISBEHAVING_ID: &str = "dev.plamenix.misbehaving";

fn stage(dir: &std::path::Path, id: &str, wasm: &[u8]) {
    std::fs::write(dir.join("plugin.wasm"), wasm).unwrap();
    std::fs::write(
        dir.join("manifest.toml"),
        format!(
            r#"
[plugin]
id = "{id}"
name = "Interceptor Fixture"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"
"#
        ),
    )
    .unwrap();
}

async fn activate(host: &PluginHost, registry: &InstanceRegistry, id: &str, wasm: &[u8]) {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path(), id, wasm);
    let version = Version::parse("1.0.0-beta").expect("version");
    let staged = load(host, &version, dir.path()).expect("load");
    activate_into_registry(
        host,
        HostState::new(&staged.manifest.plugin.id, "1.0.0-beta"),
        &staged,
        registry,
    )
    .await
    .expect("activate");
    // The tempdir must outlive the load, not the instance: the
    // component is compiled into memory by then.
    drop(dir);
}

fn registration(plugin_id: &str, point: ExtensionPoint, priority: u16) -> InterceptorRegistration {
    InterceptorRegistration {
        plugin_id: plugin_id.to_owned(),
        point,
        priority,
        purpose: None,
    }
}

#[tokio::test]
async fn a_plugin_can_refuse_an_operation() {
    // The whole reason interceptors are a separate surface from events.
    let host = PluginHost::new().expect("host");
    let instances = InstanceRegistry::new();
    activate(&host, &instances, HELLO_ID, HELLO_WASM).await;

    let interception = intercept_one(
        &instances,
        HELLO_ID,
        ExtensionPoint::QueryExecuting,
        r#"{"sql":"DROP TABLE customers"}"#,
    )
    .await;

    match interception.verdict {
        Verdict::Cancel { reason } => assert!(
            reason.contains("DROP TABLE"),
            "the reason reaches the user, so it has to say something: {reason}",
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn the_same_plugin_allows_what_it_does_not_object_to() {
    // Without this, a plugin that cancelled everything would pass the
    // test above for the wrong reason.
    let host = PluginHost::new().expect("host");
    let instances = InstanceRegistry::new();
    activate(&host, &instances, HELLO_ID, HELLO_WASM).await;

    let interception = intercept_one(
        &instances,
        HELLO_ID,
        ExtensionPoint::QueryExecuting,
        r#"{"sql":"SELECT * FROM customers"}"#,
    )
    .await;

    assert_eq!(interception.verdict, Verdict::Proceed);
}

#[tokio::test]
async fn a_plugin_can_rewrite_the_context_it_was_given() {
    // The transform path — formatter plugins rewriting a buffer.
    let host = PluginHost::new().expect("host");
    let instances = InstanceRegistry::new();
    activate(&host, &instances, HELLO_ID, HELLO_WASM).await;

    let interception = intercept_one(
        &instances,
        HELLO_ID,
        ExtensionPoint::EditorSaving,
        r#"{"tabId":"t1","buffer":"SELECT 1"}"#,
    )
    .await;

    match interception.verdict {
        Verdict::Replace { context_json } => {
            assert!(
                context_json.contains("SELECT 1 -- formatted"),
                "the replacement should carry the plugin's edit: {context_json}",
            );
        }
        other => panic!("expected a replacement, got {other:?}"),
    }
}

#[tokio::test]
async fn a_replacement_propagates_to_the_rest_of_the_chain() {
    // Chain semantics, not single-call semantics: the second plugin
    // must see what the first one produced, or `Replace` is just a
    // fancy `Proceed`.
    let host = PluginHost::new().expect("host");
    let instances = InstanceRegistry::new();
    activate(&host, &instances, HELLO_ID, HELLO_WASM).await;

    // The same plugin twice: its editor.saving handler appends a
    // marker, so running it twice must append twice. One registration
    // would not distinguish "propagated" from "ran once".
    let registrations = [
        registration(HELLO_ID, ExtensionPoint::EditorSaving, 100),
        registration(HELLO_ID, ExtensionPoint::EditorSaving, 200),
    ];

    let (verdict, steps) = run_chain(
        &instances,
        &registrations,
        ExtensionPoint::EditorSaving,
        r#"{"tabId":"t1","buffer":"SELECT 1"}"#,
    )
    .await;

    assert_eq!(steps.len(), 2);
    match verdict {
        Verdict::Replace { context_json } => assert_eq!(
            context_json.matches("-- formatted").count(),
            2,
            "the second handler did not see the first one's output: {context_json}",
        ),
        other => panic!("expected a replacement, got {other:?}"),
    }
}

#[tokio::test]
async fn a_cancel_stops_the_chain_where_it_happened() {
    let host = PluginHost::new().expect("host");
    let instances = InstanceRegistry::new();
    activate(&host, &instances, HELLO_ID, HELLO_WASM).await;
    activate(&host, &instances, MISBEHAVING_ID, MISBEHAVING_WASM).await;

    // hello refuses DROP first; the trapping plugin sits behind it and
    // must never be reached.
    let registrations = [
        registration(HELLO_ID, ExtensionPoint::QueryExecuting, 100),
        registration(MISBEHAVING_ID, ExtensionPoint::QueryExecuting, 200),
    ];

    let (verdict, steps) = run_chain(
        &instances,
        &registrations,
        ExtensionPoint::QueryExecuting,
        r#"{"sql":"DROP TABLE customers"}"#,
    )
    .await;

    assert!(matches!(verdict, Verdict::Cancel { .. }));
    assert_eq!(
        steps.len(),
        1,
        "the chain kept going after a cancel: {steps:?}",
    );
    assert_eq!(steps[0].plugin_id, HELLO_ID);
}

#[tokio::test]
async fn a_trapping_interceptor_is_skipped_rather_than_blocking_the_user() {
    // Fail-open. A plugin crashing must not be able to stop someone
    // running a query against their own database.
    let host = PluginHost::new().expect("host");
    let instances = InstanceRegistry::new();
    activate(&host, &instances, MISBEHAVING_ID, MISBEHAVING_WASM).await;

    let interception = intercept_one(
        &instances,
        MISBEHAVING_ID,
        ExtensionPoint::QueryExecuting,
        "{}",
    )
    .await;

    assert_eq!(interception.verdict, Verdict::Proceed);
    let failure = interception
        .failure
        .expect("the trap should be reported, not swallowed");
    assert_eq!(failure.kind, FailureKind::Trapped);
}

#[tokio::test]
async fn a_cancel_with_no_reason_is_not_allowed_to_block_anything() {
    // A refusal the user cannot act on is worse than no refusal: they
    // cannot tell which plugin did it or what to change.
    let host = PluginHost::new().expect("host");
    let instances = InstanceRegistry::new();
    activate(&host, &instances, MISBEHAVING_ID, MISBEHAVING_WASM).await;

    let interception = intercept_one(
        &instances,
        MISBEHAVING_ID,
        ExtensionPoint::RowDeleting,
        r#"{"table":"customers","pk":"1"}"#,
    )
    .await;

    assert_eq!(interception.verdict, Verdict::Proceed);
    assert!(
        interception.failure.is_none(),
        "an empty reason is a misbehaving plugin, not a failed call",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interceptor_that_never_returns_is_preempted() {
    // The 500ms chain budget lives in the TypeScript chain, but it can
    // only work if the host call underneath it actually ends. Without
    // the interactive deadline here, a spinning interceptor would hold
    // the store forever and the budget would time out against a call
    // that never released it.
    use plamenix_plugin_host::EpochTicker;

    let host = PluginHost::new().expect("host");
    let instances = InstanceRegistry::new();
    let ticker = EpochTicker::spawn(host.engine().clone());
    activate(&host, &instances, MISBEHAVING_ID, MISBEHAVING_WASM).await;

    let interception = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        intercept_one(
            &instances,
            MISBEHAVING_ID,
            ExtensionPoint::CellCommitting,
            "{}",
        ),
    )
    .await
    .expect("the interceptor never returned — the deadline did not fire");

    assert_eq!(interception.verdict, Verdict::Proceed);
    assert_eq!(
        interception.failure.expect("a failure").kind,
        FailureKind::Deadline,
    );

    ticker.stop().await;
}

#[tokio::test]
async fn repeated_interceptor_traps_exhaust_the_crash_budget() {
    // The interceptor path must not be a way to trap forever without
    // ever being disabled — the same hole the event path had.
    let host = PluginHost::new().expect("host");
    let instances = InstanceRegistry::new();
    activate(&host, &instances, MISBEHAVING_ID, MISBEHAVING_WASM).await;

    let supervisor = Supervisor::new();
    supervisor
        .register(MISBEHAVING_ID, RestartPolicy::Transient)
        .expect("register");
    supervisor
        .mark_active(MISBEHAVING_ID, std::time::Instant::now())
        .expect("mark active");

    // Default budget is 3 crashes in 60s.
    for _ in 0..4 {
        let _ = intercept_one_supervised(
            &instances,
            &supervisor,
            MISBEHAVING_ID,
            ExtensionPoint::QueryExecuting,
            "{}",
        )
        .await;
    }

    assert_eq!(
        supervisor.status(MISBEHAVING_ID).expect("status"),
        Some(PluginStatus::Disabled),
    );
}

#[tokio::test]
async fn a_plugin_with_no_instance_is_not_charged_for_anything() {
    // UI-only plugins legitimately have no wasm half. Treating that as
    // a crash would disable them for existing.
    let instances = InstanceRegistry::new();
    let supervisor = Supervisor::new();
    supervisor
        .register("ui.only", RestartPolicy::Transient)
        .expect("register");
    supervisor
        .mark_active("ui.only", std::time::Instant::now())
        .expect("mark active");

    for _ in 0..5 {
        let (interception, decision) = intercept_one_supervised(
            &instances,
            &supervisor,
            "ui.only",
            ExtensionPoint::QueryExecuting,
            "{}",
        )
        .await;
        assert_eq!(interception.verdict, Verdict::Proceed);
        assert!(interception.failure.is_none());
        assert!(decision.is_none());
    }

    assert_eq!(
        supervisor.status("ui.only").expect("status"),
        Some(PluginStatus::Active),
    );
}

#[tokio::test]
async fn registrations_survive_a_round_trip_through_the_registry() {
    // The shells read this to build their chain handlers, so what goes
    // in has to come back out in resolved order.
    let registry = InterceptorRegistry::new();
    registry
        .register(registration(
            "b.plugin",
            ExtensionPoint::QueryExecuting,
            300,
        ))
        .unwrap();
    registry
        .register(registration(
            "a.plugin",
            ExtensionPoint::QueryExecuting,
            100,
        ))
        .unwrap();
    registry
        .register(registration("c.plugin", ExtensionPoint::EditorSaving, 100))
        .unwrap();

    let query = registry.for_point(ExtensionPoint::QueryExecuting).unwrap();
    assert_eq!(
        query
            .iter()
            .map(|r| r.plugin_id.as_str())
            .collect::<Vec<_>>(),
        ["a.plugin", "b.plugin"],
    );
    assert_eq!(registry.all().unwrap().len(), 3);
}
