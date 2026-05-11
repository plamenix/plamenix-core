//! Phase C end-to-end integration test.
//!
//! Loads `tests/fixtures/hello-plugin.wasm` — a real WASM Component
//! built from `examples/hello-plugin/` — activates it through the
//! standard pipeline, and asserts the host's log sink captured the
//! plugin's `host.log` call.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use plamenix_plugin_host::{
    ActivationOutcome, HostState, LogLevel, LogSink, PluginHost, activate_with_state, load,
};
use semver::Version;

const HELLO_WASM_BYTES: &[u8] = include_bytes!("fixtures/hello-plugin.wasm");

#[tokio::test(flavor = "multi_thread")]
async fn hello_plugin_activates_and_logs() {
    let host = PluginHost::new().expect("engine");
    let dir = tempfile::tempdir().unwrap();

    std::fs::write(dir.path().join("plugin.wasm"), HELLO_WASM_BYTES).unwrap();
    let manifest = r#"
[plugin]
id = "org.plamenix.hello"
name = "Hello"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
wasm = "plugin.wasm"
"#;
    std::fs::write(dir.path().join("manifest.toml"), manifest).unwrap();

    let staged = load(&host, &Version::parse("1.0.0-beta").unwrap(), dir.path()).unwrap();

    let sink: LogSink = Arc::new(Mutex::new(Vec::new()));
    let state =
        HostState::new(&staged.manifest.plugin.id, "1.0.0-beta").with_log_sink(Arc::clone(&sink));

    let outcome = activate_with_state(&host, state, &staged).await.unwrap();
    assert!(
        matches!(outcome, ActivationOutcome::Ok),
        "expected Ok activation, got {outcome:?}",
    );

    let logs = sink.lock().unwrap().clone();
    assert_eq!(logs.len(), 1, "plugin should have logged exactly once");
    let log = &logs[0];
    assert_eq!(log.level, LogLevel::Info);
    assert!(
        log.message.contains("hello from plugin"),
        "unexpected message: {}",
        log.message,
    );
    assert!(
        log.message.contains("1.0.0-beta"),
        "host-version round-trip lost: {}",
        log.message,
    );
}
