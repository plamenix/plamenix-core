//! `plamenix keygen` + `plamenix sign` — Ed25519 signing CLI surface
//! (I7.15).
//!
//! Keys live on disk as raw 32-byte secret-key files (no encoding).
//! That's the simplest portable shape; tooling that needs ASCII
//! transport (CI secrets, env vars) can hex-encode the bytes
//! externally. The on-disk file is mode `0o600` to discourage
//! accidental sharing.
//!
//! Companion verifying-key file (`.pub`) is written alongside the
//! secret key with mode `0o644` so users can publish it without
//! exposing the secret half.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use plamenix_plugin_host::{PluginSignature, sign_archive};
use rand::rngs::OsRng;
use thiserror::Error;

/// Errors emitted by the keygen / sign subcommands.
#[derive(Debug, Error)]
pub enum SignCliError {
    #[error("output file already exists: {0}")]
    OutputExists(PathBuf),
    #[error("secret key file does not exist: {0}")]
    KeyMissing(PathBuf),
    #[error("secret key file has wrong length (expected 32 bytes, got {0})")]
    KeyWrongLength(usize),
    #[error("signing failed: {0}")]
    SigningFailed(#[from] plamenix_plugin_host::PluginError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Generates an Ed25519 keypair + writes the secret to `path` and the
/// verifying half to `<path>.pub`. Refuses to clobber existing files.
///
/// # Errors
///
/// See [`SignCliError`].
pub fn keygen(path: &Path) -> Result<KeygenOutput, SignCliError> {
    if path.exists() {
        return Err(SignCliError::OutputExists(path.to_path_buf()));
    }
    let pub_path = path.with_extension(format!(
        "{}.pub",
        path.extension().and_then(|s| s.to_str()).unwrap_or("key"),
    ));
    if pub_path.exists() {
        return Err(SignCliError::OutputExists(pub_path));
    }
    let key = SigningKey::generate(&mut OsRng);
    let secret_bytes = key.to_bytes();
    let public_bytes = key.verifying_key().to_bytes();
    write_with_mode(path, &secret_bytes, 0o600)?;
    write_with_mode(&pub_path, hex::encode(public_bytes).as_bytes(), 0o644)?;
    Ok(KeygenOutput {
        secret_path: path.to_path_buf(),
        public_path: pub_path,
        public_key_hex: hex::encode(public_bytes),
    })
}

/// Result of [`keygen`] — paths + the public key hex for convenience.
#[derive(Debug)]
pub struct KeygenOutput {
    pub secret_path: PathBuf,
    pub public_path: PathBuf,
    pub public_key_hex: String,
}

/// Signs `archive` in-place using the secret key stored at `key_path`.
/// `key_id` is the optional human label embedded in the signature.
///
/// # Errors
///
/// See [`SignCliError`].
pub fn sign(
    archive: &Path,
    key_path: &Path,
    key_id: &str,
) -> Result<PluginSignature, SignCliError> {
    if !key_path.is_file() {
        return Err(SignCliError::KeyMissing(key_path.to_path_buf()));
    }
    let bytes = fs::read(key_path)?;
    let key_bytes: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| SignCliError::KeyWrongLength(bytes.len()))?;
    let key = SigningKey::from_bytes(&key_bytes);
    Ok(sign_archive(archive, &key, key_id)?)
}

fn write_with_mode(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let _ = mode; // mode parameter used only on Unix below.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path)?;
        file.write_all(bytes)?;
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plamenix_plugin_host::{VerificationOutcome, verify_archive, write_plx};
    use tempfile::tempdir;

    fn manifest_text() -> &'static str {
        r#"
[plugin]
id = "sign.cli.test"
name = "Sign CLI"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"

[runtime]
restart_policy = "transient"

[permissions]
required = []
optional = []
"#
    }

    fn build_plx(dir: &Path) -> PathBuf {
        let plug = dir.join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(plug.join("manifest.toml"), manifest_text()).unwrap();
        fs::write(plug.join("ui.mjs"), "// ui").unwrap();
        let plx = dir.join("test.plx");
        write_plx(&plug, &plx).unwrap();
        plx
    }

    #[test]
    fn keygen_writes_secret_and_public() {
        let dir = tempdir().unwrap();
        let secret = dir.path().join("k.key");
        let out = keygen(&secret).unwrap();
        assert!(out.secret_path.is_file());
        assert!(out.public_path.is_file());
        assert_eq!(fs::read(&out.secret_path).unwrap().len(), 32);
        // Public key is hex (64 chars), no newline.
        assert_eq!(out.public_key_hex.len(), 64);
        let public_text = fs::read_to_string(&out.public_path).unwrap();
        assert_eq!(public_text, out.public_key_hex);
    }

    #[test]
    fn keygen_refuses_to_clobber() {
        let dir = tempdir().unwrap();
        let secret = dir.path().join("k.key");
        fs::write(&secret, b"existing").unwrap();
        let err = keygen(&secret).unwrap_err();
        assert!(matches!(err, SignCliError::OutputExists(_)));
    }

    #[test]
    fn sign_with_generated_key_verifies() {
        let dir = tempdir().unwrap();
        let secret = dir.path().join("k.key");
        let kg = keygen(&secret).unwrap();
        let plx = build_plx(dir.path());
        let sig = sign(&plx, &kg.secret_path, "cli-test").unwrap();
        assert_eq!(sig.public_key, kg.public_key_hex);
        let outcome = verify_archive(&plx).unwrap();
        let VerificationOutcome::SelfSigned {
            public_key_hex,
            key_id,
            ..
        } = outcome;
        assert_eq!(public_key_hex, kg.public_key_hex);
        assert_eq!(key_id, "cli-test");
    }

    #[test]
    fn sign_errors_on_missing_key_file() {
        let dir = tempdir().unwrap();
        let plx = build_plx(dir.path());
        let nope = dir.path().join("no.key");
        let err = sign(&plx, &nope, "k").unwrap_err();
        assert!(matches!(err, SignCliError::KeyMissing(_)));
    }

    #[test]
    fn sign_errors_on_wrong_length_key() {
        let dir = tempdir().unwrap();
        let secret = dir.path().join("k.key");
        fs::write(&secret, b"too short").unwrap();
        let plx = build_plx(dir.path());
        let err = sign(&plx, &secret, "k").unwrap_err();
        assert!(matches!(err, SignCliError::KeyWrongLength(_)));
    }
}
