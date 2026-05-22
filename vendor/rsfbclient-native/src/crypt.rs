//! Database encryption-key callback registration.
//!
//! Wraps the `fb_database_crypt_callback` C entry point that upstream
//! rsfbclient 0.26 does not surface. Hosts construct an
//! `ICryptKeyCallback`-shaped vtable on their side (the layout matches
//! `fb_c_api.h`) and pass a `*mut c_void` pointing at it; this module
//! loads the fbclient binary, resolves the symbol, and registers the
//! callback for the rest of the process lifetime.
//!
//! The Library handle is intentionally leaked: Firebird stores the
//! function-pointer machinery internally and unloading the library
//! mid-process would invalidate it.

use std::ffi::OsStr;
use std::fmt;
use std::os::raw::c_void;

/// Errors returned by [`register_crypt_callback`].
#[derive(Debug)]
pub enum CryptCallbackError {
    /// `libloading::Library::new` could not open the supplied path,
    /// or the `fb_database_crypt_callback` symbol is missing
    /// (Firebird ≤ 2.5).
    Load(libloading::Error),
    /// `fb_database_crypt_callback` returned a non-zero status. The
    /// value is the engine's `ISC_STATUS` short reply.
    Status(isize),
}

impl fmt::Display for CryptCallbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(err) => write!(f, "load fb_database_crypt_callback: {err}"),
            Self::Status(s) => write!(f, "fb_database_crypt_callback returned status {s}"),
        }
    }
}

impl std::error::Error for CryptCallbackError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Load(err) => Some(err),
            Self::Status(_) => None,
        }
    }
}

impl From<libloading::Error> for CryptCallbackError {
    fn from(value: libloading::Error) -> Self {
        Self::Load(value)
    }
}

/// Registers a Firebird `ICryptKeyCallback` for the process.
///
/// `lib_path` should point at the same `fbclient` binary
/// `IBaseDynLoading::with_client` opens — registration is per-loaded-
/// image, but dlopen returns the same handle when called twice with
/// an identical path.
///
/// `callback` is a `*mut c_void` pointer to a host-owned struct
/// whose layout matches Firebird's `ICryptKeyCallback` vtable. The
/// host keeps the struct `'static`; this function does not own it.
///
/// # Errors
///
/// - [`CryptCallbackError::Load`] when the library cannot be loaded
///   or `fb_database_crypt_callback` is missing.
/// - [`CryptCallbackError::Status`] when the engine reports a
///   non-zero status from the registration call.
pub fn register_crypt_callback(
    lib_path: &OsStr,
    callback: *mut c_void,
) -> Result<(), CryptCallbackError> {
    // SAFETY: `libloading::Library::new` opens a dynamic library by
    // path. The caller is responsible for handing in the same path
    // they passed to `IBaseDynLoading::with_client`. dlopen is
    // idempotent for identical paths so this reuses the existing
    // image rather than mapping a second copy.
    let lib = unsafe { libloading::Library::new(lib_path) }?;

    // SAFETY: Firebird's documented C signature is
    // `ISC_STATUS fb_database_crypt_callback(ISC_STATUS*, void*)`.
    // Calling with a null status pointer is documented as legal —
    // the engine allocates status space when needed. The callback
    // pointer's referenced memory is owned by the caller and must
    // outlive the process; we only forward the pointer.
    let func: libloading::Symbol<
        '_,
        unsafe extern "C" fn(*mut isize, *mut c_void) -> isize,
    > = unsafe { lib.get(b"fb_database_crypt_callback\0") }?;

    // SAFETY: invariants documented above.
    let status = unsafe { func(std::ptr::null_mut(), callback) };
    if status != 0 {
        return Err(CryptCallbackError::Status(status));
    }

    // Keep the library mapped: Firebird holds onto the callback
    // pointer for the rest of the process lifetime. Dropping the
    // Library would unmap the image and invalidate the storage the
    // engine reads from during attach.
    std::mem::forget(lib);
    Ok(())
}
