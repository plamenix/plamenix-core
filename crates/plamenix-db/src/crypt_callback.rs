//! Bridge that registers a Firebird `ICryptKeyCallback` with the
//! `fbclient` library so at-rest-encrypted databases can attach using
//! a runtime-supplied key.
//!
//! The FFI call into `fb_database_crypt_callback` lives in the
//! vendored `rsfbclient-native` crate (see
//! `plamenix-core/vendor/rsfbclient-native/src/crypt.rs`); this module
//! owns the Rust-side machinery: a process-global key slot and a
//! `'static` `ICryptKeyCallback` vtable that satisfies the Firebird
//! cloop ABI.
//!
//! Registration is idempotent: the first successful call wires the
//! callback for the rest of the process lifetime. Subsequent attaches
//! call [`set_key`] just before opening a session — the engine queries
//! the registered callback during the attach handshake, copies the
//! current slot into its buffer, and uses those bytes to decrypt
//! database pages.
//!
//! # Limitations
//!
//! - **Process-global key slot.** The Firebird C API installs a single
//!   callback per process. If two concurrent attaches need *different*
//!   keys, the last writer wins. An IDE with one active connection at
//!   a time is not affected; the desktop's session map serialises
//!   attaches so cross-talk is bounded by the lock held in
//!   `crate::rsfb::RsfbDriver::connect`.
//! - **No status propagation.** The callback returns the copied byte
//!   count; failure to allocate or absent key results in a return of
//!   `0`, which Firebird treats as "no key available" and fails the
//!   attach with a clear engine error.
//! - **Pure-Rust mode.** Skipped entirely (this module compiles only
//!   under the `native` feature). The pure-Rust backend can't speak
//!   to fbclient plugins anyway.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::os::raw::c_int;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::error::DbError;

/// Slot holding the most recently configured key bytes. Cleared on
/// successful attach to avoid leaving secrets resident in memory
/// longer than necessary (best-effort — see "Limitations" above).
static KEY_SLOT: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Flips to `true` the first time `fb_database_crypt_callback` is
/// invoked successfully. Subsequent calls short-circuit so we don't
/// re-leak the library handle inside rsfbclient-native.
static REGISTERED: OnceLock<()> = OnceLock::new();

/// Updates the process-global key slot. Called by the host edition
/// just before a connect that needs a runtime key. Bytes are
/// cloned so the caller can drop its buffer immediately.
pub fn set_key(bytes: &[u8]) {
    if let Ok(mut slot) = KEY_SLOT.lock() {
        slot.clear();
        slot.extend_from_slice(bytes);
    }
}

/// Clears the process-global key slot. Hosts call this after a
/// successful attach (or any attach failure) so the bytes do not
/// remain in memory between sessions.
pub fn clear_key() {
    if let Ok(mut slot) = KEY_SLOT.lock() {
        slot.clear();
    }
}

/// Registers the callback once for the given fbclient path. Repeated
/// calls with any path return `Ok(())` immediately after the first
/// successful registration — the engine only honours one callback per
/// process.
///
/// # Errors
///
/// Fails when the library cannot be loaded or the
/// `fb_database_crypt_callback` symbol is missing (Firebird 2.5 or
/// earlier). Both surface as [`DbError::Driver`] with the source error
/// chained into the message so the host can log it verbatim.
pub fn register_with(path: &Path) -> Result<(), DbError> {
    if REGISTERED.get().is_some() {
        return Ok(());
    }

    // SAFETY: `CALLBACK` is `'static` and `Sync`; rsfbclient-native's
    // helper forwards the pointer to `fb_database_crypt_callback` and
    // leaks its `Library` handle so the engine keeps a stable image
    // for the rest of the process lifetime.
    let callback_ptr = &raw const CALLBACK as *mut c_void;
    rsfbclient_native::crypt::register_crypt_callback(path.as_os_str(), callback_ptr).map_err(
        |err| match err {
            rsfbclient_native::crypt::CryptCallbackError::Load(_) => DbError::Driver(format!(
                "register fb_database_crypt_callback: {err}",
            )),
            rsfbclient_native::crypt::CryptCallbackError::Status(_) => {
                DbError::Driver(err.to_string())
            }
        },
    )?;

    let _ = REGISTERED.set(());
    tracing::info!(
        path = %path.display(),
        "registered ICryptKeyCallback with fbclient"
    );
    Ok(())
}

/// `ICryptKeyCallback` vtable, layout-compatible with Firebird's
/// `fb_c_api.h` declaration. The vtable starts with a single
/// `cloopDummy` pointer slot that Firebird's cloop runtime uses for
/// its own bookkeeping; we leave it null. `version` matches
/// `ICryptKeyCallback_VERSION == 3` from the same header.
#[repr(C)]
struct ICryptKeyCallbackVTable {
    cloop_dummy: *const c_void,
    version: usize,
    callback: unsafe extern "C" fn(
        *mut ICryptKeyCallback,
        u32,
        *const c_void,
        u32,
        *mut c_void,
    ) -> u32,
    dummy1: unsafe extern "C" fn(*mut ICryptKeyCallback, *mut c_void),
    dummy2: unsafe extern "C" fn(*mut ICryptKeyCallback),
    get_hash_length: unsafe extern "C" fn(*mut ICryptKeyCallback, *mut c_void) -> c_int,
    get_hash_data:
        unsafe extern "C" fn(*mut ICryptKeyCallback, *mut c_void, *mut c_void),
}

#[repr(C)]
struct ICryptKeyCallback {
    cloop_dummy: *const c_void,
    vtable: *const ICryptKeyCallbackVTable,
}

// SAFETY: every field of `ICryptKeyCallback` and its vtable is
// `'static` and read-only (function pointers + null padding). The
// callback functions themselves read from `KEY_SLOT` under a mutex,
// so the struct is genuinely thread-safe.
unsafe impl Sync for ICryptKeyCallback {}
unsafe impl Sync for ICryptKeyCallbackVTable {}

unsafe extern "C" fn cb_callback(
    _self: *mut ICryptKeyCallback,
    _data_length: u32,
    _data: *const c_void,
    buffer_length: u32,
    buffer: *mut c_void,
) -> u32 {
    let Ok(slot) = KEY_SLOT.lock() else {
        return 0;
    };
    if slot.is_empty() || buffer.is_null() {
        return 0;
    }
    let copy_len = slot.len().min(buffer_length as usize);
    // SAFETY: `buffer` was sized for `buffer_length` bytes by the
    // engine; `slot.as_ptr()` points to `slot.len() >= copy_len`
    // bytes; the two regions are disjoint Rust-owned vs. caller-
    // owned memory.
    unsafe {
        std::ptr::copy_nonoverlapping(slot.as_ptr(), buffer.cast::<u8>(), copy_len);
    }
    u32::try_from(copy_len).unwrap_or(u32::MAX)
}

unsafe extern "C" fn cb_dummy1(_self: *mut ICryptKeyCallback, _status: *mut c_void) {}
unsafe extern "C" fn cb_dummy2(_self: *mut ICryptKeyCallback) {}
unsafe extern "C" fn cb_hash_length(
    _self: *mut ICryptKeyCallback,
    _status: *mut c_void,
) -> c_int {
    0
}
unsafe extern "C" fn cb_hash_data(
    _self: *mut ICryptKeyCallback,
    _status: *mut c_void,
    _hash: *mut c_void,
) {
}

static VTABLE: ICryptKeyCallbackVTable = ICryptKeyCallbackVTable {
    cloop_dummy: std::ptr::null(),
    version: 3,
    callback: cb_callback,
    dummy1: cb_dummy1,
    dummy2: cb_dummy2,
    get_hash_length: cb_hash_length,
    get_hash_data: cb_hash_data,
};

static CALLBACK: ICryptKeyCallback = ICryptKeyCallback {
    cloop_dummy: std::ptr::null(),
    vtable: &VTABLE,
};
