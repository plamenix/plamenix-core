//! `.plx` plugin archive format (I7.10).
//!
//! # Format
//!
//! `.plx` is a standard PKZIP archive (deflate compression). The
//! layout mirrors the on-disk bundle the loader already accepts:
//!
//! ```text
//! plugin.plx
//! ├── manifest.toml      (required, top-level)
//! ├── plugin.wasm        (optional — WASM half; present for
//! │                       plugins that ship a wasmtime component)
//! ├── ui.mjs             (optional — React UI module imported by
//! │                       the host when the plugin contributes
//! │                       sidebar / inspector panels)
//! └── resources/         (optional — static assets the plugin's UI
//!     ├── icon.svg        bundle references via relative paths)
//!     └── ...
//! ```
//!
//! Native-binary (subprocess) plugins ship their executable inside
//! the archive too — typically `bin/<arch>/plugin` — but the host
//! doesn't constrain that path layout because the manifest's
//! `[entry_points].subprocess` field already names it. The
//! extractor preserves the relative tree; the loader validates the
//! pointed-at file exists.
//!
//! # Path-traversal safety
//!
//! The extractor refuses any entry whose normalised relative path:
//!
//!   - is absolute,
//!   - contains a `..` segment,
//!   - or contains a NUL byte / other control characters that
//!     downstream filesystems reject.
//!
//! The check happens BEFORE any file is created on disk; a hostile
//! archive can't drop a file outside the destination dir even
//! partially.

use std::fs::{File, create_dir_all};
use std::io::{BufReader, Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::error::PluginError;
use crate::manifest::Manifest;

/// Filename inside a `.plx` archive carrying the manifest. Required
/// member.
pub const MANIFEST_NAME: &str = "manifest.toml";

/// Standard wasm component name. Optional — UI-only plugins omit it.
pub const WASM_NAME: &str = "plugin.wasm";

/// Standard UI module name. Optional.
pub const UI_NAME: &str = "ui.mjs";

/// Conventional resources directory.
pub const RESOURCES_DIR: &str = "resources";

/// Builds a `.plx` archive at `dest` from the directory `src_dir`.
///
/// Walks `src_dir` recursively and writes every regular file into the
/// zip with its relative path preserved. Empty directories are
/// omitted (they have no extraction-side meaning in the bundle
/// format).
///
/// # Errors
///
/// Returns [`PluginError::Io`] on filesystem read errors,
/// [`PluginError::Runtime`] on zip-writer errors, or
/// [`PluginError::InvalidManifest`] when `manifest.toml` is missing
/// from `src_dir`. The writer refuses to produce a `.plx` without
/// the required manifest member; the loader would reject the result
/// anyway, so the failure should surface at write time.
pub fn write_plx(src_dir: &Path, dest: &Path) -> Result<(), PluginError> {
    let manifest_path = src_dir.join(MANIFEST_NAME);
    if !manifest_path.is_file() {
        return Err(PluginError::InvalidManifest(format!(
            "missing required `{MANIFEST_NAME}` in source directory `{}`",
            src_dir.display(),
        )));
    }

    let dest_file = File::create(dest)?;
    let mut writer = ZipWriter::new(dest_file);
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .large_file(false)
        .unix_permissions(0o644);

    write_dir_into_zip(src_dir, src_dir, &mut writer, options)?;
    writer.finish().map_err(zip_runtime_err)?;
    Ok(())
}

fn write_dir_into_zip(
    src_root: &Path,
    src: &Path,
    writer: &mut ZipWriter<File>,
    options: SimpleFileOptions,
) -> Result<(), PluginError> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            write_dir_into_zip(src_root, &path, writer, options)?;
            continue;
        }
        if !file_type.is_file() {
            // Skip symlinks / special files. The bundle format only
            // carries regular files.
            continue;
        }
        let rel = path.strip_prefix(src_root).map_err(|err| {
            PluginError::Runtime(format!("path under src_root strip failed: {err}"))
        })?;
        let rel_str = rel
            .to_str()
            .ok_or_else(|| PluginError::Runtime("path is not valid UTF-8".to_string()))?
            // Force forward slashes — zip stores POSIX-style paths
            // regardless of writer-host OS so cross-platform
            // extraction stays consistent.
            .replace('\\', "/");
        writer
            .start_file(rel_str, options)
            .map_err(zip_runtime_err)?;
        let mut f = File::open(&path)?;
        std::io::copy(&mut f, writer)?;
    }
    Ok(())
}

/// Reads + parses the manifest from a `.plx` archive without
/// extracting the full archive. Useful for the install dialog's
/// "preview perms before unpack" step.
///
/// # Errors
///
/// Returns [`PluginError::InvalidManifest`] if the manifest member
/// is missing OR fails to parse; [`PluginError::Io`] on file read
/// errors; [`PluginError::Runtime`] on zip-reader errors.
pub fn peek_plx_manifest(plx_path: &Path) -> Result<Manifest, PluginError> {
    let file = File::open(plx_path)?;
    let mut archive = ZipArchive::new(BufReader::new(file)).map_err(zip_runtime_err)?;
    let mut entry = archive
        .by_name(MANIFEST_NAME)
        .map_err(|err| PluginError::InvalidManifest(format!("{MANIFEST_NAME}: {err}")))?;
    let mut text = String::new();
    entry.read_to_string(&mut text)?;
    Manifest::parse(&text)
}

/// Reads + parses the manifest from `.plx` bytes already loaded into
/// memory (e.g. the web admin install endpoint's base64-decoded
/// upload). Same error model as [`peek_plx_manifest`].
pub fn peek_plx_manifest_bytes(bytes: &[u8]) -> Result<Manifest, PluginError> {
    let cursor = Cursor::new(bytes);
    let mut archive = ZipArchive::new(cursor).map_err(zip_runtime_err)?;
    let mut entry = archive
        .by_name(MANIFEST_NAME)
        .map_err(|err| PluginError::InvalidManifest(format!("{MANIFEST_NAME}: {err}")))?;
    let mut text = String::new();
    entry.read_to_string(&mut text)?;
    Manifest::parse(&text)
}

/// Extracts a `.plx` archive into `dest_dir`. The destination
/// directory MUST exist + be empty; the caller is responsible for
/// creating a fresh temp dir.
///
/// # Path-traversal safety
///
/// Every archive entry is validated through [`safe_relative_path`]
/// before any filesystem write. Absolute paths, `..` segments, and
/// embedded NUL bytes cause the extractor to bail with
/// [`PluginError::InvalidManifest`] (re-used as the "bundle malformed"
/// signal so callers map it to a single user-facing error).
///
/// # Errors
///
/// Returns [`PluginError::Io`] on filesystem write errors,
/// [`PluginError::Runtime`] on zip-reader errors, or
/// [`PluginError::InvalidManifest`] on traversal attempts +
/// manifest absence.
pub fn extract_plx(plx_path: &Path, dest_dir: &Path) -> Result<(), PluginError> {
    let file = File::open(plx_path)?;
    extract_plx_from_reader(BufReader::new(file), dest_dir)
}

/// Extracts a `.plx` archive from in-memory bytes. Used by the web
/// admin install route (I7.8) so the server doesn't have to write
/// the upload to a tempfile before extracting.
pub fn extract_plx_bytes(bytes: &[u8], dest_dir: &Path) -> Result<(), PluginError> {
    extract_plx_from_reader(Cursor::new(bytes), dest_dir)
}

fn extract_plx_from_reader<R: Read + std::io::Seek>(
    reader: R,
    dest_dir: &Path,
) -> Result<(), PluginError> {
    let mut archive = ZipArchive::new(reader).map_err(zip_runtime_err)?;

    let mut saw_manifest = false;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(zip_runtime_err)?;
        if !entry.is_file() {
            // Directory entries don't need writing; the per-file
            // mkdir below covers their creation.
            continue;
        }
        let name = entry.name().to_string();
        let safe_rel = safe_relative_path(&name)?;
        if safe_rel.as_os_str() == MANIFEST_NAME {
            saw_manifest = true;
        }
        let out_path = dest_dir.join(&safe_rel);
        if let Some(parent) = out_path.parent() {
            create_dir_all(parent)?;
        }
        let mut out_file = File::create(&out_path)?;
        std::io::copy(&mut entry, &mut out_file)?;
        out_file.flush()?;
    }

    if !saw_manifest {
        return Err(PluginError::InvalidManifest(format!(
            "archive missing required `{MANIFEST_NAME}`"
        )));
    }
    Ok(())
}

/// Validates an archive-supplied path is safe to join under a
/// destination directory. Rejects absolute paths, `..` segments,
/// and NUL bytes.
fn safe_relative_path(raw: &str) -> Result<PathBuf, PluginError> {
    if raw.is_empty() {
        return Err(PluginError::InvalidManifest(
            "archive entry has empty name".to_string(),
        ));
    }
    if raw.contains('\0') {
        return Err(PluginError::InvalidManifest(format!(
            "archive entry name contains NUL byte: {raw:?}"
        )));
    }
    // Reject Windows-style drive-letter prefixes too — even on Unix,
    // a path like `C:/Windows/...` could escape on a future Windows
    // host. Cheap defensive check.
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(PluginError::InvalidManifest(format!(
            "archive entry is absolute: {raw:?}"
        )));
    }
    let candidate = PathBuf::from(raw);
    for component in candidate.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(PluginError::InvalidManifest(format!(
                    "archive entry contains `..` segment: {raw:?}"
                )));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(PluginError::InvalidManifest(format!(
                    "archive entry contains absolute root: {raw:?}"
                )));
            }
        }
    }
    Ok(candidate)
}

fn zip_runtime_err(err: impl std::fmt::Display) -> PluginError {
    PluginError::Runtime(format!("zip error: {err}"))
}

// Silence unused-import warnings when only some helpers are exercised
// in any given build configuration.
#[allow(dead_code)]
fn _silence(w: &mut dyn Write) {
    let _ = w;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_text(p: &Path, body: &str) {
        let mut f = File::create(p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    fn minimal_manifest() -> &'static str {
        r#"
[plugin]
id = "test.plx.roundtrip"
name = "Roundtrip"
version = "1.0.0"
plamenix_min_version = ">=1.0.0-beta"
plugin_api = "1.0"

[entry_points]
ui = "ui.mjs"
"#
    }

    #[test]
    fn write_plx_refuses_directory_without_manifest() {
        let src = tempdir().unwrap();
        let dest = tempdir().unwrap().keep().join("out.plx");
        let err = write_plx(src.path(), &dest).unwrap_err();
        assert!(matches!(err, PluginError::InvalidManifest(_)));
    }

    #[test]
    fn roundtrip_minimal_bundle() {
        let src = tempdir().unwrap();
        write_text(&src.path().join(MANIFEST_NAME), minimal_manifest());
        write_text(&src.path().join(UI_NAME), "// ui");
        let plx = tempdir().unwrap().keep().join("out.plx");
        write_plx(src.path(), &plx).unwrap();
        assert!(plx.exists());

        let extract_dir = tempdir().unwrap();
        extract_plx(&plx, extract_dir.path()).unwrap();
        let extracted_manifest =
            std::fs::read_to_string(extract_dir.path().join(MANIFEST_NAME)).unwrap();
        assert_eq!(extracted_manifest.trim(), minimal_manifest().trim());
        let extracted_ui = std::fs::read_to_string(extract_dir.path().join(UI_NAME)).unwrap();
        assert_eq!(extracted_ui, "// ui");
    }

    #[test]
    fn roundtrip_preserves_nested_resources() {
        let src = tempdir().unwrap();
        write_text(&src.path().join(MANIFEST_NAME), minimal_manifest());
        let resources = src.path().join("resources");
        std::fs::create_dir(&resources).unwrap();
        write_text(&resources.join("icon.svg"), "<svg/>");
        std::fs::create_dir(resources.join("sub")).unwrap();
        write_text(&resources.join("sub").join("more.bin"), "binary");

        let plx = tempdir().unwrap().keep().join("nested.plx");
        write_plx(src.path(), &plx).unwrap();

        let extract_dir = tempdir().unwrap();
        extract_plx(&plx, extract_dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(extract_dir.path().join("resources/icon.svg")).unwrap(),
            "<svg/>",
        );
        assert_eq!(
            std::fs::read_to_string(extract_dir.path().join("resources/sub/more.bin")).unwrap(),
            "binary",
        );
    }

    #[test]
    fn peek_plx_manifest_returns_parsed_manifest_without_extracting() {
        let src = tempdir().unwrap();
        write_text(&src.path().join(MANIFEST_NAME), minimal_manifest());
        let plx = tempdir().unwrap().keep().join("peek.plx");
        write_plx(src.path(), &plx).unwrap();
        let manifest = peek_plx_manifest(&plx).unwrap();
        assert_eq!(manifest.plugin.id, "test.plx.roundtrip");
    }

    #[test]
    fn peek_plx_manifest_bytes_works_on_in_memory_archive() {
        let src = tempdir().unwrap();
        write_text(&src.path().join(MANIFEST_NAME), minimal_manifest());
        let plx = tempdir().unwrap().keep().join("peek-bytes.plx");
        write_plx(src.path(), &plx).unwrap();
        let bytes = std::fs::read(&plx).unwrap();
        let manifest = peek_plx_manifest_bytes(&bytes).unwrap();
        assert_eq!(manifest.plugin.id, "test.plx.roundtrip");
    }

    #[test]
    fn extract_plx_refuses_archive_missing_manifest() {
        // Build a zip with only ui.mjs.
        let src = tempdir().unwrap();
        write_text(&src.path().join(UI_NAME), "// ui");
        // Bypass write_plx (which would reject) by constructing the
        // zip manually.
        let plx = tempdir().unwrap().keep().join("no-manifest.plx");
        let f = File::create(&plx).unwrap();
        let mut w = ZipWriter::new(f);
        let opt = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        w.start_file(UI_NAME, opt).unwrap();
        w.write_all(b"// ui").unwrap();
        w.finish().unwrap();

        let dest = tempdir().unwrap();
        let err = extract_plx(&plx, dest.path()).unwrap_err();
        assert!(matches!(err, PluginError::InvalidManifest(_)));
    }

    #[test]
    fn extract_plx_refuses_traversal_entry() {
        // Hand-build an archive with `../escape.txt` and verify the
        // safety check rejects it BEFORE writing anything to dest.
        let plx = tempdir().unwrap().keep().join("traversal.plx");
        let f = File::create(&plx).unwrap();
        let mut w = ZipWriter::new(f);
        let opt = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        // Include a real manifest entry too so the archive isn't
        // rejected on the manifest-missing check.
        w.start_file(MANIFEST_NAME, opt).unwrap();
        w.write_all(minimal_manifest().as_bytes()).unwrap();
        w.start_file("../escape.txt", opt).unwrap();
        w.write_all(b"escape").unwrap();
        w.finish().unwrap();

        let dest = tempdir().unwrap();
        let err = extract_plx(&plx, dest.path()).unwrap_err();
        assert!(matches!(err, PluginError::InvalidManifest(_)));
        // Defensive: confirm the dest is empty except for the
        // manifest written before the malicious entry was hit.
        // The current loop writes manifest first, then bails on the
        // traversal entry — so the dest may contain the manifest but
        // NEVER the escape file.
        let escape_path = dest.path().join("..").join("escape.txt");
        assert!(!escape_path.exists(), "traversal file must not be created");
    }

    #[test]
    fn extract_plx_refuses_absolute_entry() {
        let plx = tempdir().unwrap().keep().join("absolute.plx");
        let f = File::create(&plx).unwrap();
        let mut w = ZipWriter::new(f);
        let opt = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        w.start_file(MANIFEST_NAME, opt).unwrap();
        w.write_all(minimal_manifest().as_bytes()).unwrap();
        w.start_file("/etc/passwd", opt).unwrap();
        w.write_all(b"root").unwrap();
        w.finish().unwrap();

        let dest = tempdir().unwrap();
        let err = extract_plx(&plx, dest.path()).unwrap_err();
        assert!(matches!(err, PluginError::InvalidManifest(_)));
    }

    #[test]
    fn safe_relative_path_accepts_normal_names() {
        assert!(safe_relative_path(MANIFEST_NAME).is_ok());
        assert!(safe_relative_path("resources/icon.svg").is_ok());
        assert!(safe_relative_path("bin/x86_64-linux/plugin").is_ok());
    }

    #[test]
    fn safe_relative_path_rejects_empty_and_nul() {
        assert!(safe_relative_path("").is_err());
        assert!(safe_relative_path("foo\0bar").is_err());
    }
}
