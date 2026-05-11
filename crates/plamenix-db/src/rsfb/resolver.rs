//! Locate the bundled `fbclient` shared library at runtime.
//!
//! Plamenix never relies on a system-installed `fbclient` for its
//! production connections. Instead, each desktop installer and web
//! container ships its own copy under `resources/fbclient/<version>/`,
//! and the driver loads it via rsfbclient's `with_dyn_load(path)`.
//!
//! This resolver picks a library path in three steps:
//!
//! 1. **Explicit override** — [`ConnectionConfig::fbclient_path`].
//! 2. **Process-wide override** — the `PLAMENIX_FBCLIENT_PATH`
//!    environment variable, useful for tests and dev shells.
//! 3. `None` — the caller is responsible for failing the connection
//!    rather than silently linking against whatever happens to be on
//!    the loader path.
//!
//! Production hosts (desktop and web) set up the bundle path at boot
//! time and pass it through `ConnectionConfig.fbclient_path`. The env
//! var exists for ad-hoc work where threading the bundle path through
//! every call site is friction.

use std::path::PathBuf;

use plamenix_types::ConnectionConfig;

/// Environment variable inspected by [`resolve_fbclient_path`] when the
/// connection config does not name an `fbclient_path` directly.
pub const FBCLIENT_PATH_ENV: &str = "PLAMENIX_FBCLIENT_PATH";

/// Picks an `fbclient` shared library to load for the given
/// connection, or returns `None` when neither the connection nor the
/// environment names one.
#[must_use]
pub fn resolve_fbclient_path(config: &ConnectionConfig) -> Option<PathBuf> {
    if let Some(explicit) = &config.fbclient_path {
        return Some(PathBuf::from(explicit));
    }
    std::env::var_os(FBCLIENT_PATH_ENV).map(PathBuf::from)
}
