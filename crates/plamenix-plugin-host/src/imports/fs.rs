//! `fs` — a plugin's own directory, and nothing else.
//!
//! The jail is this module's, not WASI's. Handing the plugin a WASI
//! preopen would have been less code and a strictly larger grant: it
//! also hands over the whole `wasi:filesystem` interface, where these
//! four functions are the entire intended surface.
//!
//! Keeping it here rather than behind [`crate::services::HostServices`]
//! is deliberate too. A path jail implemented twice is a path jail with
//! two chances to be wrong, and the failure mode is a sandbox escape
//! rather than a broken feature.
//!
//! ## What "inside the directory" means
//!
//! [`resolve`] refuses anything absolute, anything with a `..`
//! component, anything with a Windows drive prefix or root, and — after
//! canonicalising what exists — anything that still lands outside the
//! root. That last step is what catches a symlink pointing out of the
//! tree, which no amount of string inspection would.

use std::path::{Component, Path, PathBuf};

use crate::bindings::plamenix::plugin::fs::{FsError, Host as FsHost};
use crate::capability::{LogicalDir, Permission};
use crate::gate::{self, Guard};
use crate::host_impl::HostState;

/// Reading needs read access to the plugin's own data directory.
const READ_GUARD: Guard = Guard::Any(&[Permission::FsReadDir(LogicalDir::PluginData)]);
/// Writing needs write access, which does not imply read.
const WRITE_GUARD: Guard = Guard::Any(&[Permission::FsWriteDir(LogicalDir::PluginData)]);

/// Largest file a plugin may read or write through this interface.
///
/// The wasm `ResourceLimiter` bounds the guest's linear memory but not
/// the host buffer a read allocates before handing it over, so without
/// a cap here a plugin could ask for a file larger than the machine.
pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Resolves a plugin-supplied path inside `root`.
///
/// # Errors
///
/// [`FsError::InvalidPath`] for anything that is not a plain relative
/// path, or that escapes `root` once symlinks are followed.
pub fn resolve(root: &Path, supplied: &str) -> Result<PathBuf, FsError> {
    if supplied.contains('\0') {
        return Err(FsError::InvalidPath("path contains a NUL byte".to_owned()));
    }

    let candidate = Path::new(supplied);
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(FsError::InvalidPath(
                    "`..` is not allowed; paths are relative to the plugin's own directory"
                        .to_owned(),
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(FsError::InvalidPath(
                    "absolute paths are not allowed; paths are relative to the plugin's own directory"
                        .to_owned(),
                ));
            }
        }
    }

    let joined = root.join(candidate);

    // String checks cannot see a symlink. Canonicalise the deepest
    // existing ancestor and confirm it is still inside the root — for a
    // file being created, that ancestor is its parent directory.
    let existing = joined
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .unwrap_or(root);
    let (Ok(real_root), Ok(real_existing)) = (root.canonicalize(), existing.canonicalize()) else {
        // A root that cannot be canonicalised does not exist yet, which
        // is not the plugin's fault and not a path-escape either.
        return Ok(joined);
    };
    if !real_existing.starts_with(&real_root) {
        return Err(FsError::InvalidPath(
            "path resolves outside the plugin's own directory".to_owned(),
        ));
    }

    Ok(joined)
}

/// The plugin's root, or the error to hand back when it has none.
fn root(state: &HostState) -> Result<PathBuf, FsError> {
    state
        .data_dir
        .clone()
        .ok_or_else(|| FsError::PermissionDenied("this plugin has no data directory".to_owned()))
}

fn io_error(err: &std::io::Error) -> FsError {
    match err.kind() {
        std::io::ErrorKind::NotFound => FsError::NotFound,
        std::io::ErrorKind::PermissionDenied => FsError::PermissionDenied(err.to_string()),
        _ => FsError::IoError(err.to_string()),
    }
}

#[async_trait::async_trait]
impl FsHost for HostState {
    async fn read_file(&mut self, path: String) -> wasmtime::Result<Result<Vec<u8>, FsError>> {
        if let Err(denial) = gate::check(self, &READ_GUARD) {
            return Ok(Err(FsError::PermissionDenied(denial.to_string())));
        }
        Ok(read_file_inner(self, &path))
    }

    async fn write_file(
        &mut self,
        path: String,
        contents: Vec<u8>,
    ) -> wasmtime::Result<Result<(), FsError>> {
        if let Err(denial) = gate::check(self, &WRITE_GUARD) {
            return Ok(Err(FsError::PermissionDenied(denial.to_string())));
        }
        Ok(write_file_inner(self, &path, &contents))
    }

    async fn list_dir(&mut self, path: String) -> wasmtime::Result<Result<Vec<String>, FsError>> {
        if let Err(denial) = gate::check(self, &READ_GUARD) {
            return Ok(Err(FsError::PermissionDenied(denial.to_string())));
        }
        Ok(list_dir_inner(self, &path))
    }

    async fn remove_file(&mut self, path: String) -> wasmtime::Result<Result<(), FsError>> {
        if let Err(denial) = gate::check(self, &WRITE_GUARD) {
            return Ok(Err(FsError::PermissionDenied(denial.to_string())));
        }
        Ok(remove_file_inner(self, &path))
    }
}

fn read_file_inner(state: &HostState, path: &str) -> Result<Vec<u8>, FsError> {
    let resolved = resolve(&root(state)?, path)?;
    let metadata = std::fs::metadata(&resolved).map_err(|err| io_error(&err))?;
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(FsError::IoError(format!(
            "file is larger than the {MAX_FILE_BYTES} byte limit"
        )));
    }
    std::fs::read(&resolved).map_err(|err| io_error(&err))
}

fn write_file_inner(state: &HostState, path: &str, contents: &[u8]) -> Result<(), FsError> {
    if contents.len() > MAX_FILE_BYTES {
        return Err(FsError::IoError(format!(
            "contents exceed the {MAX_FILE_BYTES} byte limit"
        )));
    }
    let resolved = resolve(&root(state)?, path)?;
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent).map_err(|err| io_error(&err))?;
    }
    std::fs::write(&resolved, contents).map_err(|err| io_error(&err))
}

fn list_dir_inner(state: &HostState, path: &str) -> Result<Vec<String>, FsError> {
    let resolved = resolve(&root(state)?, path)?;
    let entries = std::fs::read_dir(&resolved).map_err(|err| io_error(&err))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| io_error(&err))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    // Deterministic order: the OS does not promise one, and a plugin
    // that iterates its own directory should not behave differently on
    // two machines.
    names.sort_unstable();
    Ok(names)
}

fn remove_file_inner(state: &HostState, path: &str) -> Result<(), FsError> {
    let resolved = resolve(&root(state)?, path)?;
    std::fs::remove_file(&resolved).map_err(|err| io_error(&err))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_plain_relative_path_resolves_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve(dir.path(), "notes/today.txt").unwrap();
        assert!(resolved.starts_with(dir.path()));
    }

    #[test]
    fn the_obvious_escapes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "../outside.txt",
            "notes/../../outside.txt",
            "/etc/passwd",
            "with\0nul",
        ] {
            assert!(
                resolve(dir.path(), bad).is_err(),
                "`{bad}` should not resolve",
            );
        }
    }

    #[test]
    fn a_leading_dot_is_fine() {
        // `./x` is not an escape and refusing it would be surprising.
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve(dir.path(), "./notes.txt").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_out_of_the_tree_is_refused() {
        // The case no amount of string inspection catches, and the
        // reason resolution canonicalises rather than just parsing.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();

        let err = resolve(dir.path(), "escape/secret.txt").unwrap_err();
        assert!(matches!(err, FsError::InvalidPath(_)), "got {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_staying_inside_the_tree_is_allowed() {
        // The control: canonicalising must not refuse every symlink,
        // only the ones that leave.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::fs::write(dir.path().join("real/data.txt"), b"ok").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("alias")).unwrap();

        assert!(resolve(dir.path(), "alias/data.txt").is_ok());
    }

    #[test]
    fn a_file_that_does_not_exist_yet_still_resolves() {
        // Writing a new file has to work; only its parent can be
        // canonicalised, which is what the ancestor walk is for.
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve(dir.path(), "brand/new/file.txt").is_ok());
    }
}
