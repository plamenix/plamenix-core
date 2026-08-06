//! `.plx` signing format + sign/verify helpers (I7.15 + I7.16).
//!
//! # Format
//!
//! Signed archives carry an extra top-level member named
//! `signature.json` alongside `manifest.toml`. The file is a UTF-8
//! JSON document with this shape:
//!
//! ```json
//! {
//!   "algorithm": "ed25519",
//!   "public_key": "<32-byte hex>",
//!   "signature": "<64-byte hex>",
//!   "signed_at": "1718572800",
//!   "key_id": "optional-string"
//! }
//! ```
//!
//! - **algorithm**: only `"ed25519"` is accepted today. The field is
//!   present so future algorithm rotations don't need a version bump.
//! - **public_key**: 32-byte Ed25519 verifying key, lower-case hex.
//! - **signature**: 64-byte Ed25519 signature, lower-case hex.
//! - **signed_at**: Unix epoch seconds as a string. Stored as a
//!   string to avoid JSON integer-precision surprises in clients
//!   that parse big numbers as floats.
//! - **key_id**: optional free-form string the signer can use to
//!   identify which key produced the signature. Hosts may surface
//!   this in the install dialog for transparency.
//!
//! # Digest
//!
//! The signature covers a SHA-256 digest of every archive member
//! EXCEPT `signature.json` itself. The digest input is built
//! deterministically:
//!
//! 1. Collect every (name, bytes) pair where `name != "signature.json"`.
//! 2. Sort the pairs by `name` (lexicographic).
//! 3. For each pair, hash:
//!    - 8 bytes: little-endian `name.len()` as `u64`
//!    - `name` bytes
//!    - 8 bytes: little-endian `bytes.len()` as `u64`
//!    - `bytes` themselves
//!
//! The length-prefixing prevents length-extension + collision
//! attacks across pairs (two archives with rearranged member
//! boundaries hash to different digests). Sorting by name ensures
//! the digest is independent of the order entries happen to land in
//! the zip's central directory.
//!
//! # Roundtrip
//!
//! ```ignore
//! use ed25519_dalek::SigningKey;
//! use rand::rngs::OsRng;
//!
//! let key = SigningKey::generate(&mut OsRng);
//! sign_archive(&plx_path, &key, Some("ci-bot"))?;
//! let outcome = verify_archive(&plx_path)?;
//! assert!(matches!(outcome, VerificationOutcome::Valid { .. }));
//! ```

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::error::PluginError;
use crate::plx::MANIFEST_NAME;

/// Filename of the signature member inside the `.plx` archive.
pub const SIGNATURE_NAME: &str = "signature.json";

/// Only-supported signing algorithm. Field exists to make future
/// rotations possible without a format bump.
pub const ALGORITHM: &str = "ed25519";

/// Sidecar JSON written into the archive's `signature.json` member.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSignature {
    /// Always `"ed25519"` today; future algorithms add new variants.
    pub algorithm: String,
    /// Lower-case hex of the 32-byte Ed25519 verifying key.
    pub public_key: String,
    /// Lower-case hex of the 64-byte Ed25519 signature.
    pub signature: String,
    /// Unix epoch seconds (as a string for cross-runtime safety).
    pub signed_at: String,
    /// Optional human-readable hint for the signing key. Hosts MAY
    /// render it in the install dialog. Defaults to empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_id: String,
}

/// Verdict returned by [`verify_archive`].
#[derive(Debug)]
pub enum VerificationOutcome {
    /// Signature verified successfully. Carries the public key + key
    /// id from the signature for the host to display.
    Valid {
        public_key_hex: String,
        key_id: String,
        signed_at: String,
    },
}

/// Computes the canonical SHA-256 digest over the archive's members
/// excluding `signature.json`. Public so the CLI can build a
/// detached signature workflow if needed.
pub fn compute_archive_digest(archive_bytes: &[u8]) -> Result<[u8; 32], PluginError> {
    let cursor = Cursor::new(archive_bytes);
    let mut archive = ZipArchive::new(cursor).map_err(zip_runtime_err)?;
    let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(zip_runtime_err)?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        if name == SIGNATURE_NAME {
            continue;
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        entries.insert(name, buf);
    }
    Ok(digest_entries(&entries))
}

fn digest_entries(entries: &BTreeMap<String, Vec<u8>>) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for (name, bytes) in entries {
        let name_bytes = name.as_bytes();
        let name_len = u64::try_from(name_bytes.len()).unwrap_or(u64::MAX);
        let body_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        hasher.update(name_len.to_le_bytes());
        hasher.update(name_bytes);
        hasher.update(body_len.to_le_bytes());
        hasher.update(bytes);
    }
    hasher.finalize().into()
}

/// Signs an in-memory archive + returns the resulting
/// [`PluginSignature`]. Does NOT modify the archive bytes —
/// composing the signed archive is the caller's job (typically via
/// [`embed_signature_into_archive`] / [`sign_archive`]).
pub fn sign_archive_bytes(
    archive_bytes: &[u8],
    signing_key: &SigningKey,
    key_id: &str,
) -> Result<PluginSignature, PluginError> {
    let digest = compute_archive_digest(archive_bytes)?;
    let sig = signing_key.sign(&digest);
    let signed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string();
    Ok(PluginSignature {
        algorithm: ALGORITHM.to_string(),
        public_key: hex::encode(signing_key.verifying_key().to_bytes()),
        signature: hex::encode(sig.to_bytes()),
        signed_at,
        key_id: key_id.to_string(),
    })
}

/// Embeds (or replaces) the `signature.json` member inside the
/// archive at `path`.
pub fn embed_signature_into_archive(
    path: &Path,
    signature: &PluginSignature,
) -> Result<(), PluginError> {
    let original = std::fs::read(path)?;
    let cursor = Cursor::new(&original);
    let mut reader = ZipArchive::new(cursor).map_err(zip_runtime_err)?;
    let temp_path = path.with_extension("plx.signing-tmp");
    let out_file = File::create(&temp_path)?;
    let mut writer = ZipWriter::new(out_file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);

    for i in 0..reader.len() {
        let mut entry = reader.by_index(i).map_err(zip_runtime_err)?;
        let name = entry.name().to_string();
        if name == SIGNATURE_NAME {
            // Skip — we'll write the fresh signature at the end.
            continue;
        }
        if !entry.is_file() {
            continue;
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        writer.start_file(name, options).map_err(zip_runtime_err)?;
        writer.write_all(&buf)?;
    }
    let payload = serde_json::to_vec_pretty(signature)
        .map_err(|err| PluginError::SignatureMalformed(err.to_string()))?;
    writer
        .start_file(SIGNATURE_NAME, options)
        .map_err(zip_runtime_err)?;
    writer.write_all(&payload)?;
    writer.finish().map_err(zip_runtime_err)?;
    std::fs::rename(&temp_path, path)?;
    Ok(())
}

/// Signs a `.plx` archive at `path` in-place using `signing_key`.
/// Replaces any pre-existing `signature.json` member.
///
/// # Errors
///
/// See [`PluginError`].
pub fn sign_archive(
    path: &Path,
    signing_key: &SigningKey,
    key_id: &str,
) -> Result<PluginSignature, PluginError> {
    let bytes = std::fs::read(path)?;
    // Sanity: confirm the archive carries the required manifest
    // BEFORE signing. Signing a malformed archive is operator
    // confusion waiting to happen.
    {
        let cursor = Cursor::new(&bytes);
        let mut archive = ZipArchive::new(cursor).map_err(zip_runtime_err)?;
        if archive.by_name(MANIFEST_NAME).is_err() {
            return Err(PluginError::InvalidManifest(format!(
                "archive missing required `{MANIFEST_NAME}`; cannot sign"
            )));
        }
    }
    let signature = sign_archive_bytes(&bytes, signing_key, key_id)?;
    embed_signature_into_archive(path, &signature)?;
    Ok(signature)
}

/// Verifies the `signature.json` inside `path` against the archive's
/// other members. Returns [`VerificationOutcome`] on success;
/// [`PluginError::SignatureMissing`] / `SignatureInvalid` /
/// `SignatureMalformed` on failure.
pub fn verify_archive(path: &Path) -> Result<VerificationOutcome, PluginError> {
    let file = File::open(path)?;
    verify_archive_from_reader(BufReader::new(file))
}

/// Verifies a `.plx` archive from in-memory bytes.
pub fn verify_archive_bytes(bytes: &[u8]) -> Result<VerificationOutcome, PluginError> {
    verify_archive_from_reader(Cursor::new(bytes))
}

fn verify_archive_from_reader<R: Read + Seek>(
    reader: R,
) -> Result<VerificationOutcome, PluginError> {
    let mut archive = ZipArchive::new(reader).map_err(zip_runtime_err)?;
    let sig_text = {
        let Ok(mut entry) = archive.by_name(SIGNATURE_NAME) else {
            return Err(PluginError::SignatureMissing);
        };
        let mut buf = String::new();
        entry.read_to_string(&mut buf)?;
        buf
    };
    let parsed: PluginSignature = serde_json::from_str(&sig_text)
        .map_err(|err| PluginError::SignatureMalformed(err.to_string()))?;
    if parsed.algorithm != ALGORITHM {
        return Err(PluginError::SignatureMalformed(format!(
            "unsupported algorithm `{}`",
            parsed.algorithm
        )));
    }
    let pk_bytes: [u8; 32] = hex::decode(&parsed.public_key)
        .map_err(|err| PluginError::SignatureMalformed(format!("public_key not hex: {err}")))?
        .try_into()
        .map_err(|_| PluginError::SignatureMalformed("public_key wrong length".to_string()))?;
    let sig_bytes: [u8; 64] = hex::decode(&parsed.signature)
        .map_err(|err| PluginError::SignatureMalformed(format!("signature not hex: {err}")))?
        .try_into()
        .map_err(|_| PluginError::SignatureMalformed("signature wrong length".to_string()))?;
    let verifying_key = VerifyingKey::from_bytes(&pk_bytes)
        .map_err(|err| PluginError::SignatureMalformed(format!("public_key invalid: {err}")))?;
    let signature = Signature::from_bytes(&sig_bytes);

    // Recompute the digest over the archive's members.
    let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(zip_runtime_err)?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        if name == SIGNATURE_NAME {
            continue;
        }
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        entries.insert(name, buf);
    }
    let digest = digest_entries(&entries);
    verifying_key
        .verify(&digest, &signature)
        .map_err(|err| PluginError::SignatureInvalid(err.to_string()))?;
    Ok(VerificationOutcome::Valid {
        public_key_hex: parsed.public_key,
        key_id: parsed.key_id,
        signed_at: parsed.signed_at,
    })
}

fn zip_runtime_err(err: impl std::fmt::Display) -> PluginError {
    PluginError::Runtime(format!("zip error: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plx::write_plx;
    use rand::rngs::OsRng;
    use std::fs;
    use tempfile::tempdir;

    fn manifest_text() -> &'static str {
        r#"
[plugin]
id = "sign.test"
name = "Sign Test"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"

[runtime]
restart_policy = "transient"
requires_subprocess = false

[permissions]
required = []
optional = []
"#
    }

    fn make_plx() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let plug = dir.path().join("p");
        fs::create_dir(&plug).unwrap();
        fs::write(plug.join("manifest.toml"), manifest_text()).unwrap();
        fs::write(plug.join("ui.mjs"), "// ui").unwrap();
        let plx = dir.path().join("out.plx");
        write_plx(&plug, &plx).unwrap();
        (dir, plx)
    }

    #[test]
    fn unsigned_archive_verifies_as_missing_signature() {
        let (_dir, plx) = make_plx();
        let err = verify_archive(&plx).unwrap_err();
        assert!(matches!(err, PluginError::SignatureMissing));
    }

    #[test]
    fn sign_then_verify_roundtrip() {
        let (_dir, plx) = make_plx();
        let key = SigningKey::generate(&mut OsRng);
        let sig = sign_archive(&plx, &key, "test-key-1").unwrap();
        assert_eq!(sig.algorithm, ALGORITHM);
        assert_eq!(sig.public_key.len(), 64); // 32 bytes hex
        assert_eq!(sig.signature.len(), 128); // 64 bytes hex
        let outcome = verify_archive(&plx).unwrap();
        let VerificationOutcome::Valid {
            public_key_hex,
            key_id,
            ..
        } = outcome;
        assert_eq!(public_key_hex, sig.public_key);
        assert_eq!(key_id, "test-key-1");
    }

    #[test]
    fn signing_replaces_prior_signature() {
        let (_dir, plx) = make_plx();
        let key_a = SigningKey::generate(&mut OsRng);
        let key_b = SigningKey::generate(&mut OsRng);
        let _sig_a = sign_archive(&plx, &key_a, "a").unwrap();
        let sig_b = sign_archive(&plx, &key_b, "b").unwrap();
        let outcome = verify_archive(&plx).unwrap();
        let VerificationOutcome::Valid { public_key_hex, key_id, .. } = outcome;
        assert_eq!(public_key_hex, sig_b.public_key);
        assert_eq!(key_id, "b");
    }

    #[test]
    fn verify_fails_when_archive_member_tampered() {
        let (dir, plx) = make_plx();
        let key = SigningKey::generate(&mut OsRng);
        sign_archive(&plx, &key, "k").unwrap();
        // Replace the ui.mjs member inside the archive by rebuilding
        // a new archive from a tampered source dir.
        let plug2 = dir.path().join("p2");
        fs::create_dir(&plug2).unwrap();
        fs::write(plug2.join("manifest.toml"), manifest_text()).unwrap();
        fs::write(plug2.join("ui.mjs"), "// tampered").unwrap();
        // Extract the signature from the original, then build a new
        // archive with tampered content + the SAME signature.
        let original_bytes = fs::read(&plx).unwrap();
        let cursor = Cursor::new(&original_bytes);
        let mut original = ZipArchive::new(cursor).unwrap();
        let mut sig_entry = original.by_name(SIGNATURE_NAME).unwrap();
        let mut sig_text = String::new();
        sig_entry.read_to_string(&mut sig_text).unwrap();
        drop(sig_entry);
        let sig: PluginSignature = serde_json::from_str(&sig_text).unwrap();

        let tampered_plx = dir.path().join("tampered.plx");
        write_plx(&plug2, &tampered_plx).unwrap();
        embed_signature_into_archive(&tampered_plx, &sig).unwrap();

        let err = verify_archive(&tampered_plx).unwrap_err();
        assert!(matches!(err, PluginError::SignatureInvalid(_)));
    }

    #[test]
    fn verify_fails_on_malformed_signature_json() {
        let (_dir, plx) = make_plx();
        let key = SigningKey::generate(&mut OsRng);
        sign_archive(&plx, &key, "k").unwrap();
        let garbage = PluginSignature {
            algorithm: "rsa".to_string(),
            public_key: "deadbeef".to_string(),
            signature: "00".to_string(),
            signed_at: "0".to_string(),
            key_id: "x".to_string(),
        };
        embed_signature_into_archive(&plx, &garbage).unwrap();
        let err = verify_archive(&plx).unwrap_err();
        assert!(matches!(err, PluginError::SignatureMalformed(_)));
    }

    #[test]
    fn compute_archive_digest_is_stable_across_runs() {
        let (_dir, plx) = make_plx();
        let bytes = fs::read(&plx).unwrap();
        let a = compute_archive_digest(&bytes).unwrap();
        let b = compute_archive_digest(&bytes).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn signing_an_archive_without_manifest_errors() {
        let dir = tempdir().unwrap();
        let plx = dir.path().join("bad.plx");
        let f = File::create(&plx).unwrap();
        let mut w = ZipWriter::new(f);
        let opt = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        w.start_file("nothing.txt", opt).unwrap();
        w.write_all(b"no manifest here").unwrap();
        w.finish().unwrap();

        let key = SigningKey::generate(&mut OsRng);
        let err = sign_archive(&plx, &key, "k").unwrap_err();
        assert!(matches!(err, PluginError::InvalidManifest(_)));
    }
}
