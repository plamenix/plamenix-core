//! Errors surfaced by the plugin host.

use std::path::PathBuf;

use thiserror::Error;

/// Errors returned by manifest parsing, plugin loading, and runtime calls.
#[derive(Debug, Error)]
pub enum PluginError {
    /// The manifest file is missing or unreadable.
    #[error("manifest missing at {0}")]
    ManifestMissing(PathBuf),

    /// The manifest contents are not valid TOML or do not match the schema.
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),

    /// A declared `entry_points.wasm` path does not exist on disk.
    #[error("plugin wasm missing at {0}")]
    WasmMissing(PathBuf),

    /// A capability string fails the grammar in `docs/capability-model.md`.
    #[error("invalid capability `{0}`: {1}")]
    InvalidCapability(String, &'static str),

    /// A required capability was not granted by the host or the user.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The plugin's `plamenix_min_version` constraint excludes the host.
    #[error("host version {host} does not satisfy plugin requirement `{required}`")]
    IncompatibleHost {
        /// The current host `SemVer`.
        host: String,
        /// The `SemVer` range the plugin required.
        required: String,
    },

    /// Wasmtime engine, component, or instance error.
    #[error("wasm runtime error: {0}")]
    Runtime(String),

    /// The archive expands to more than the extraction limits allow.
    ///
    /// Compression ratios are unbounded, so a small `.plx` can declare
    /// or decompress to far more than the disk can hold. Refused rather
    /// than truncated: a partially extracted plugin is not a plugin.
    #[error("archive too large: {0}")]
    ArchiveTooLarge(String),

    /// IO error reading bundle files.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Supervisor was asked about a plugin id that was never
    /// registered (or has been forgotten).
    #[error("plugin not registered with supervisor: {0}")]
    PluginNotFound(String),

    /// I7.15 — `.plx` archive signature is malformed (bad JSON,
    /// missing fields, unsupported algorithm).
    #[error("plugin signature malformed: {0}")]
    SignatureMalformed(String),

    /// I7.16 — `.plx` archive carries no `signature.json` member.
    #[error("plugin archive is unsigned")]
    SignatureMissing,

    /// I7.16 — signature failed cryptographic verification (tampered
    /// archive OR wrong public key).
    #[error("plugin signature failed verification: {0}")]
    SignatureInvalid(String),
}
