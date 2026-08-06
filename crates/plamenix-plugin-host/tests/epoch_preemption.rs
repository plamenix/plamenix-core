//! Does the epoch ticker actually preempt a plugin?
//!
//! The unit tests in `src/epoch.rs` check arithmetic — that
//! `Interactive` is 100ms and converts to 10 ticks. All of them pass
//! whether or not anything ever increments the engine's epoch, which
//! is precisely the state the host shipped in: the deadlines were set
//! on every store and nothing ever advanced the clock they were
//! measured against, so a plugin that looped ran until the process
//! died.
//!
//! These drive the real thing: spawn the ticker, let it advance the
//! epoch past a store's deadline, and require the next call into that
//! plugin to trap.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use plamenix_plugin_host::{
    EpochTicker, HostState, InstanceRegistry, PluginHost, activate_into_registry, load,
};
use semver::Version;

const HELLO_WASM_BYTES: &[u8] = include_bytes!("fixtures/hello-plugin.wasm");

fn write_hello_plugin(dir: &std::path::Path) {
    std::fs::write(dir.join("plugin.wasm"), HELLO_WASM_BYTES).unwrap();
    std::fs::write(
        dir.join("manifest.toml"),
        r#"
[plugin]
id = "dev.plamenix.epoch"
name = "Epoch Fixture"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"
"#,
    )
    .unwrap();
}

async fn live_instance(
    host: &PluginHost,
    registry: &InstanceRegistry,
    dir: &std::path::Path,
) -> std::sync::Arc<plamenix_plugin_host::PluginInstance> {
    write_hello_plugin(dir);
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
    registry
        .get(&staged.manifest.plugin.id)
        .expect("registry lock")
        .expect("instance registered")
}

#[tokio::test]
async fn a_running_ticker_preempts_a_call_past_its_deadline() {
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");

    // Hold the ticker for the length of the test, exactly as a shell
    // holds it for the length of the process.
    let ticker = EpochTicker::spawn(host.engine().clone());

    let instance = live_instance(&host, &registry, dir.path()).await;

    {
        let mut store = instance.lock_store().await;
        // One tick of headroom, then let the ticker spend it.
        store.set_epoch_deadline(1);
    }
    tokio::time::sleep(Duration::from_millis(80)).await;

    let mut store = instance.lock_store().await;
    let err = instance
        .bindings()
        .plamenix_plugin_plugin()
        .call_handle_event(&mut *store, "plamenix.test", "{}")
        .await
        .expect_err("the call should have been preempted");

    let rendered = format!("{err:?}").to_lowercase();
    assert!(
        rendered.contains("epoch")
            || rendered.contains("interrupt")
            || rendered.contains("deadline"),
        "expected an epoch-deadline trap, got: {err:?}",
    );

    ticker.stop().await;
}

#[tokio::test]
async fn a_call_inside_its_deadline_still_runs() {
    // The preemption must not be so eager that ordinary calls trip it.
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");

    let ticker = EpochTicker::spawn(host.engine().clone());
    let instance = live_instance(&host, &registry, dir.path()).await;

    let mut store = instance.lock_store().await;
    // Generous headroom relative to the 10ms tick.
    store.set_epoch_deadline(1_000);
    instance
        .bindings()
        .plamenix_plugin_plugin()
        .call_handle_event(&mut *store, "plamenix.test", "{}")
        .await
        .expect("a call well inside its deadline must not be preempted");

    drop(store);
    ticker.stop().await;
}

#[tokio::test]
async fn without_a_ticker_the_deadline_never_fires() {
    // Documents the bug this feature fixes: the deadline is set on
    // every store, but with nothing advancing the epoch it is inert,
    // and the same call that traps above succeeds here.
    let host = PluginHost::new().expect("host");
    let registry = InstanceRegistry::new();
    let dir = tempfile::tempdir().expect("tempdir");

    let instance = live_instance(&host, &registry, dir.path()).await;

    {
        let mut store = instance.lock_store().await;
        store.set_epoch_deadline(1);
    }
    tokio::time::sleep(Duration::from_millis(80)).await;

    let mut store = instance.lock_store().await;
    instance
        .bindings()
        .plamenix_plugin_plugin()
        .call_handle_event(&mut *store, "plamenix.test", "{}")
        .await
        .expect("with no ticker running, nothing advances the epoch");
}
