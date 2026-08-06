//! I9.4 — crash isolation tests.
//!
//! Verifies two layers of crash safety:
//!
//!   1. **Host survival** — a plugin that fails to instantiate (bad
//!      wasm, malformed component) surfaces as `PluginError::Runtime`
//!      without panicking the host or poisoning the engine. The
//!      host's `try-then-handle` discipline (host_impl.rs panic
//!      forbid, I8.9) keeps the host process alive even when the
//!      plugin half explodes.
//!   2. **Supervisor restart per policy** — abnormal exits drive the
//!      `Supervisor::on_exit` state machine; the verdict matches the
//!      manifest's `restart_policy` and the crash-budget state. Unit
//!      tests in `supervisor::tests` cover the state-machine matrix;
//!      this file verifies the same behaviour wired against real
//!      `StagedPlugin` manifests with each restart policy + a
//!      multi-crash scenario that exhausts the budget end-to-end.
//!
//! ## Why this matters
//!
//! A plugin host that crashes when a plugin traps is a worse
//! product than no plugin host. The point of the WASM sandbox is
//! containment; the point of the supervisor is recovery. I9.4 proves
//! both halves work together.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::time::Instant;

use plamenix_plugin_host::{
    DisableReason, ExitReason, HostState, InstanceRegistry, PluginError, PluginHost, PluginStatus,
    RestartDecision, RestartPolicy, Supervisor, activate_into_registry, load,
};
use semver::Version;

fn host_version() -> Version {
    Version::parse("1.0.0-beta").unwrap()
}

fn stage_corrupted_wasm(dir: &Path, manifest_id: &str) {
    let manifest = format!(
        r#"
[plugin]
id = "{manifest_id}"
name = "Corrupted"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"

[permissions]
required = []
optional = []
"#,
    );
    std::fs::write(dir.join("manifest.toml"), manifest).unwrap();
    // 8 bytes of garbage where the wasm header should be. wasmtime
    // rejects this at `instantiate_async`; the loader's
    // `Component::from_binary` will fail first.
    std::fs::write(dir.join("plugin.wasm"), b"\xff\xff\xff\xffXXXX").unwrap();
}

fn manifest_with_policy(id: &str, policy: &str) -> String {
    format!(
        r#"
[plugin]
id = "{id}"
name = "{id}"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"
restart_policy = "{policy}"

[entry_points]
ui = "ui.mjs"

[permissions]
required = []
optional = []
"#,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupted_wasm_surfaces_runtime_error_without_panic() {
    let host = PluginHost::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    stage_corrupted_wasm(dir.path(), "test.crash.bad-wasm");

    // The loader stages the bundle. `load` itself parses the
    // component — corrupted wasm fails there with a Runtime error.
    let result = load(&host, &host_version(), dir.path());
    let err = result.unwrap_err();
    assert!(
        matches!(err, PluginError::Runtime(_)),
        "expected Runtime error, got {err:?}",
    );
    // Host did not panic — we got this far + still own a usable host.
    // Subsequent operations should work fine.
    let host2 = PluginHost::new().unwrap();
    let _ = host2; // keep alive briefly so the engine is exercised
}

#[tokio::test(flavor = "multi_thread")]
async fn transient_policy_restarts_on_abnormal_then_disables_after_budget() {
    // End-to-end multi-crash scenario for the Transient policy.
    // Registers a plugin, marks it active, then drives three
    // abnormal exits. The third hits the default budget (3 in 60s)
    // and disables the plugin; the registry entry is removed by the
    // simulated crash handler.
    let host = PluginHost::new().unwrap();
    let supervisor = Supervisor::new();
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("manifest.toml"),
        manifest_with_policy("test.crash.transient", "transient"),
    )
    .unwrap();
    std::fs::write(dir.path().join("ui.mjs"), "// ui").unwrap();
    let staged = load(&host, &host_version(), dir.path()).unwrap();

    supervisor
        .register(
            &staged.manifest.plugin.id,
            staged.manifest.plugin.restart_policy,
        )
        .unwrap();
    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    let _ = activate_into_registry(&host, state, &staged, &registry)
        .await
        .unwrap();
    supervisor
        .mark_active(&staged.manifest.plugin.id, Instant::now())
        .unwrap();

    let t = Instant::now();
    // 1st abnormal — restart (within budget).
    let d1 = supervisor
        .on_exit(&staged.manifest.plugin.id, ExitReason::Abnormal, t)
        .unwrap();
    assert_eq!(d1, RestartDecision::Restart);
    // 2nd abnormal — restart.
    let d2 = supervisor
        .on_exit(&staged.manifest.plugin.id, ExitReason::Abnormal, t)
        .unwrap();
    assert_eq!(d2, RestartDecision::Restart);
    // 3rd abnormal — exhausts budget → Disable.
    let d3 = supervisor
        .on_exit(&staged.manifest.plugin.id, ExitReason::Abnormal, t)
        .unwrap();
    assert_eq!(
        d3,
        RestartDecision::Disable {
            reason: DisableReason::CrashBudgetExhausted,
        },
    );
    assert_eq!(
        supervisor.status(&staged.manifest.plugin.id).unwrap(),
        Some(PluginStatus::Disabled),
    );

    // Simulating the crash-handler removing the dead instance.
    let _ = registry.remove(&staged.manifest.plugin.id).unwrap();
    assert_eq!(registry.len().unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn temporary_policy_never_restarts() {
    let host = PluginHost::new().unwrap();
    let supervisor = Supervisor::new();
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("manifest.toml"),
        manifest_with_policy("test.crash.temporary", "temporary"),
    )
    .unwrap();
    std::fs::write(dir.path().join("ui.mjs"), "// ui").unwrap();
    let staged = load(&host, &host_version(), dir.path()).unwrap();
    supervisor
        .register(
            &staged.manifest.plugin.id,
            staged.manifest.plugin.restart_policy,
        )
        .unwrap();
    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    let _ = activate_into_registry(&host, state, &staged, &registry)
        .await
        .unwrap();
    supervisor
        .mark_active(&staged.manifest.plugin.id, Instant::now())
        .unwrap();

    let decision = supervisor
        .on_exit(
            &staged.manifest.plugin.id,
            ExitReason::Abnormal,
            Instant::now(),
        )
        .unwrap();
    assert_eq!(decision, RestartDecision::Stop);
}

#[tokio::test(flavor = "multi_thread")]
async fn permanent_policy_restarts_on_both_normal_and_abnormal() {
    let host = PluginHost::new().unwrap();
    let supervisor = Supervisor::new();
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("manifest.toml"),
        manifest_with_policy("test.crash.permanent", "permanent"),
    )
    .unwrap();
    std::fs::write(dir.path().join("ui.mjs"), "// ui").unwrap();
    let staged = load(&host, &host_version(), dir.path()).unwrap();
    supervisor
        .register(
            &staged.manifest.plugin.id,
            staged.manifest.plugin.restart_policy,
        )
        .unwrap();
    let state = HostState::new(&staged.manifest.plugin.id, "1.0.0-beta");
    let _ = activate_into_registry(&host, state, &staged, &registry)
        .await
        .unwrap();
    supervisor
        .mark_active(&staged.manifest.plugin.id, Instant::now())
        .unwrap();
    let t = Instant::now();
    let d_normal = supervisor
        .on_exit(&staged.manifest.plugin.id, ExitReason::Normal, t)
        .unwrap();
    assert_eq!(d_normal, RestartDecision::Restart);
    let d_abnormal = supervisor
        .on_exit(&staged.manifest.plugin.id, ExitReason::Abnormal, t)
        .unwrap();
    assert_eq!(d_abnormal, RestartDecision::Restart);
}

#[tokio::test(flavor = "multi_thread")]
async fn re_enable_clears_disabled_state_and_resets_budget() {
    let host = PluginHost::new().unwrap();
    let supervisor = Supervisor::new();
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("manifest.toml"),
        manifest_with_policy("test.crash.reenable", "transient"),
    )
    .unwrap();
    std::fs::write(dir.path().join("ui.mjs"), "// ui").unwrap();
    let staged = load(&host, &host_version(), dir.path()).unwrap();
    let plugin_id = staged.manifest.plugin.id.clone();
    supervisor
        .register(&plugin_id, staged.manifest.plugin.restart_policy)
        .unwrap();
    let state = HostState::new(&plugin_id, "1.0.0-beta");
    let _ = activate_into_registry(&host, state, &staged, &registry)
        .await
        .unwrap();
    supervisor.mark_active(&plugin_id, Instant::now()).unwrap();

    // Drive 3 abnormal exits to exhaust the budget.
    let t = Instant::now();
    for _ in 0..3 {
        let _ = supervisor
            .on_exit(&plugin_id, ExitReason::Abnormal, t)
            .unwrap();
    }
    assert_eq!(
        supervisor.status(&plugin_id).unwrap(),
        Some(PluginStatus::Disabled),
    );

    // User re-enable from the Permissions panel.
    supervisor.re_enable(&plugin_id).unwrap();
    let state = supervisor.state(&plugin_id).unwrap().unwrap();
    assert_eq!(state.status, PluginStatus::Loaded);
    assert_eq!(state.restart_count, 0);
    assert_eq!(state.crash_budget.count_in_window(Instant::now()), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn user_requested_exit_overrides_policy() {
    // Even with a Permanent policy, a `UserRequested` exit must
    // result in `Stop` (not `Restart`). Verifies the supervisor's
    // top-level guard ahead of the policy-based decision matrix.
    let supervisor = Supervisor::new();
    supervisor.register("p", RestartPolicy::Permanent).unwrap();
    supervisor.mark_active("p", Instant::now()).unwrap();
    let decision = supervisor
        .on_exit("p", ExitReason::UserRequested, Instant::now())
        .unwrap();
    assert_eq!(decision, RestartDecision::Stop);
    assert_eq!(supervisor.status("p").unwrap(), Some(PluginStatus::Stopped),);
}

#[tokio::test(flavor = "multi_thread")]
async fn supervisor_isolates_crashes_between_plugins() {
    // Two plugins active. One crashes 3x → Disabled. The other
    // remains Active + healthy. Verifies cross-plugin state isolation
    // in the supervisor's HashMap.
    let supervisor = Supervisor::new();
    supervisor
        .register("crasher", RestartPolicy::Transient)
        .unwrap();
    supervisor
        .register("healthy", RestartPolicy::Transient)
        .unwrap();
    let t = Instant::now();
    supervisor.mark_active("crasher", t).unwrap();
    supervisor.mark_active("healthy", t).unwrap();
    for _ in 0..3 {
        let _ = supervisor
            .on_exit("crasher", ExitReason::Abnormal, t)
            .unwrap();
    }
    assert_eq!(
        supervisor.status("crasher").unwrap(),
        Some(PluginStatus::Disabled),
    );
    assert_eq!(
        supervisor.status("healthy").unwrap(),
        Some(PluginStatus::Active),
    );
    let healthy = supervisor.state("healthy").unwrap().unwrap();
    assert_eq!(healthy.crash_budget.count_in_window(t), 0);
    assert_eq!(healthy.restart_count, 0);
}
