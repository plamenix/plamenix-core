//! `plamenix-cli` — command-line tooling for plugin authors.
//!
//! Each subcommand maps to one step of the plugin authoring loop:
//!
//! - **`new`** (I7.11) — scaffolds a fresh plugin directory with
//!   manifest, Rust crate, and TypeScript UI stubs.
//! - **`build`** (I7.12) — compiles the wasm half + builds the UI
//!   bundle + assembles a `.plx`.
//! - **`pack` / `install` / `validate`** (I7.13) — bundle, install
//!   into a running host, dry-run a manifest. `pack` + `validate`
//!   land in M1; `install` is stubbed (HTTP client deferred to M2).
//!
//! The library half (this crate) is what the binary calls into; the
//! split keeps the subcommand logic unit-testable without spawning
//! a process.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod build;
pub mod install;
pub mod pack;
pub mod scaffold;
pub mod sign;
pub mod validate;

pub use build::{
    BuildError, BuildOptions, BuildOutput, PackageManager, build_plugin, detect_package_manager,
};
pub use install::{InstallError, InstallOutput, install_plugin};
pub use pack::{PackError, PackOutput, pack_plugin};
pub use scaffold::{
    NewPluginOptions, ScaffoldError, ScaffoldResult, ensure_safe_plugin_id, render_template,
    scaffold_new_plugin,
};
pub use sign::{KeygenOutput, SignCliError, keygen, sign};
pub use validate::{ValidateError, ValidationReport, render_report, validate_target};
