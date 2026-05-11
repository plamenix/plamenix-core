//! Native-binary subprocess activator.
//!
//! Plugins that opt out of the WASM sandbox set
//! `runtime.requires_subprocess = true` in their manifest and ship a
//! native executable at `entry_points.subprocess`. The host spawns the
//! binary with the plugin id, the host version, and the granted
//! permissions on argv; the plugin writes one JSON activation line to
//! stdout and any diagnostic chatter to stderr.
//!
//! ## Protocol v1
//!
//! Argv:
//!
//! ```text
//! <binary> --activate \
//!     --plugin-id <id> \
//!     --host-version <semver> \
//!     --permissions <json-array-of-strings>
//! ```
//!
//! The first non-empty line on stdout MUST be one of:
//!
//! * `{"status":"ok"}` — activation succeeded.
//! * `{"status":"failed","message":"<reason>"}` — activation reported a
//!   user-visible failure.
//!
//! stderr lines are forwarded to `tracing::info!` keyed on the plugin
//! id, so plugins can use stderr for diagnostic logging without
//! corrupting the activation protocol.
//!
//! A non-zero exit code is treated as failure regardless of stdout.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

use crate::activator::ActivationOutcome;
use crate::capability::Permission;
use crate::error::PluginError;
use crate::loader::StagedPlugin;
use crate::manifest::Manifest;

const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum SubprocessResult {
    Ok,
    Failed { message: String },
}

/// Activates a plugin that requires subprocess execution.
///
/// # Errors
///
/// * [`PluginError::MissingSubprocessCapability`] when the manifest's
///   `runtime.subprocess` capability is absent.
/// * [`PluginError::MissingSubprocessEntry`] when no
///   `entry_points.subprocess` path is declared.
/// * [`PluginError::SubprocessMissing`] when the declared binary is
///   not present on disk.
/// * [`PluginError::SubprocessFailed`] when the process cannot be
///   spawned, exceeds the activation timeout, or exits non-zero.
/// * [`PluginError::SubprocessProtocol`] when the process exits cleanly
///   but its stdout does not match the activation contract.
pub async fn activate_subprocess(
    host_version: &str,
    staged: &StagedPlugin,
) -> Result<ActivationOutcome, PluginError> {
    let manifest = &staged.manifest;
    require_subprocess_capability(manifest)?;

    let Some(rel) = manifest.entry_points.subprocess.as_ref() else {
        return Err(PluginError::MissingSubprocessEntry);
    };
    let binary: PathBuf = staged.bundle_dir.join(rel);
    if !binary.exists() {
        return Err(PluginError::SubprocessMissing(binary));
    }

    let permission_strings: Vec<String> = manifest
        .permissions
        .required
        .iter()
        .chain(manifest.permissions.optional.iter())
        .map(Permission::to_string)
        .collect();
    let permissions_json =
        serde_json::to_string(&permission_strings).map_err(|err| PluginError::Runtime(err.to_string()))?;

    let spawn = Command::new(&binary)
        .arg("--activate")
        .arg("--plugin-id")
        .arg(&manifest.plugin.id)
        .arg("--host-version")
        .arg(host_version)
        .arg("--permissions")
        .arg(permissions_json)
        .output();

    let spawn_result = timeout(ACTIVATION_TIMEOUT, spawn).await;
    let output = match spawn_result {
        Err(_) => {
            return Err(PluginError::SubprocessFailed {
                code: None,
                message: format!(
                    "subprocess did not exit within {} seconds",
                    ACTIVATION_TIMEOUT.as_secs()
                ),
            });
        }
        Ok(Err(err)) => {
            return Err(PluginError::SubprocessFailed {
                code: None,
                message: format!("failed to spawn {}: {err}", binary.display()),
            });
        }
        Ok(Ok(output)) => output,
    };

    forward_stderr(&manifest.plugin.id, &output.stderr);

    if !output.status.success() {
        return Err(PluginError::SubprocessFailed {
            code: output.status.code(),
            message: format!("{} exited unsuccessfully", binary.display()),
        });
    }

    let stdout_text = std::str::from_utf8(&output.stdout)
        .map_err(|err| PluginError::SubprocessProtocol(format!("stdout was not UTF-8: {err}")))?;
    let first_line = stdout_text
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| PluginError::SubprocessProtocol("plugin produced no stdout".into()))?;

    let parsed: SubprocessResult = serde_json::from_str(first_line.trim())
        .map_err(|err| PluginError::SubprocessProtocol(format!("bad activation json: {err}")))?;

    Ok(match parsed {
        SubprocessResult::Ok => ActivationOutcome::Ok,
        SubprocessResult::Failed { message } => ActivationOutcome::Failed(message),
    })
}

fn require_subprocess_capability(manifest: &Manifest) -> Result<(), PluginError> {
    if manifest.permissions.grants(&Permission::RuntimeSubprocess) {
        Ok(())
    } else {
        Err(PluginError::MissingSubprocessCapability)
    }
}

fn forward_stderr(plugin_id: &str, stderr: &[u8]) {
    let text = String::from_utf8_lossy(stderr);
    for line in text.lines() {
        if !line.is_empty() {
            tracing::info!(plugin = %plugin_id, "{line}");
        }
    }
}
