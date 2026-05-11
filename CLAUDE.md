# plamenix-core

Shared Rust crates consumed by both Plamenix editions. This `CLAUDE.md`
loads on demand when work happens inside this repo. Global Plamenix
conventions live in the parent workspace `CLAUDE.md`; this file only
covers concerns specific to this repository.

## What lives here

- `crates/plamenix-types/` — value types shared by every other crate.
- `crates/plamenix-db/` — `DbDriver` trait + rsfbclient-backed
  implementation (the first of the three swap-point traits).
- `crates/plamenix-plugin-host/` — WASM Component Model plugin runtime
  (manifest parsing, capability grammar, wasmtime engine wrapper,
  bundle loader). Phase A: load + validate; Phase B: activate; Phase C:
  sample plugin + end-to-end test. Plugins that set
  `runtime.requires_subprocess` in the manifest route through the
  native-binary subprocess activator instead of the WASM pipeline; the
  protocol is documented at the top of `src/subprocess.rs`.
- `crates/plamenix-plugin-sdk/` — plugin authoring SDK. Ships the
  `plamenix:plugin` WIT contract (verbatim copy) plus a subprocess
  protocol helper (`subprocess::run`) for native plugins. External
  authors depend on this crate, not on `plamenix-plugin-host`.
- `crates/plamenix-secrets/` — OS keyring wrapper (macOS Keychain,
  Windows Credential Manager, Linux Secret Service) behind the
  `SecretStore` trait. `KeyringStore` for production, `InMemoryStore`
  for tests.
- `crates/plamenix-profiles/` — saved connection profiles. `Profile`
  struct + `JsonFileStore` (atomic JSON-on-disk) + the
  `resolve_connection_config` helper that fetches secrets via
  `plamenix-secrets` and merges runtime overrides at connect time.
- More crates accrete here as concrete use cases arrive. Crates are
  added in the PR that first needs them, not pre-scaffolded.

## What does not live here

- Application code (Tauri shell, web server, React UI) — those live in
  the desktop and web repos.
- Public plugin SDK lives here once it is implemented
  (`crates/plamenix-plugin-sdk`), but plugin examples live in the
  separate `plamenix-plugin-template` repo when that is created.

## Build commands

```sh
just build      # cargo build --workspace
just test       # cargo test --workspace
just fmt        # cargo fmt --all
just lint       # cargo clippy --workspace --all-targets -- -D warnings
just doc        # cargo doc --workspace --no-deps --open
```

`rust-toolchain.toml` pins the compiler to **1.95** stable. The toolchain
is downloaded automatically on first build.

## Code style

- Functions over methods over traits over generics, in that order of
  preference. Reach for the next level only when concretely justified.
- Errors typed with `thiserror`. No `anyhow` here — this is library
  territory, callers need typed variants to recover from.
- No `unwrap`, `expect`, or `panic!` in production code. Tests and
  examples are exempt.
- `#![forbid(unsafe_code)]` at every crate root. The single anticipated
  exception is `plamenix-plugin-host` (wasmtime FFI), which will narrow
  the forbid to specific modules and require `SAFETY:` comments.
- Lint configuration lives in the workspace `Cargo.toml` `[workspace.lints]`
  section. Do not add per-crate lint overrides without justification.
- Public API earns full rustdoc: summary, `# Errors` when applicable,
  `# Examples` when usage is non-obvious. `# Safety` is mandatory for
  any `unsafe` function. Internal pub items get a single summary line.

## Conventions specific to this repo

- Workspace crates are versioned in lockstep. Bump every crate's version
  together via the workspace `[workspace.package]` block; never set
  per-crate `version = "..."` outside `Cargo.lock`.
- New crates go under `crates/`. Add the member to the workspace
  `members = ["crates/*"]` glob automatically; do not list explicitly.
- The `plamenix-types` crate is the only crate that may be a direct
  dependency of every other crate. Other crates should not depend on
  each other unless one is a domain trait host (DB driver, plugin host,
  secret store) for the other.

## What goes in `plamenix-types`

Only data shapes. No IO, no async, no driver code, no `dyn` traits. If a
candidate type needs an external dependency beyond `serde`, `serde_json`,
`uuid`, or `thiserror`, it belongs in a feature-specific crate instead.

## Per-crate `lib.rs` opening

Every crate's `lib.rs` opens with a `//!` block that explains the crate's
purpose, mental model, and how to extend it. Crate-level docs are the
landing page on `docs.rs`; they earn the same care as the README.

## Things to ask before doing

- Adding a new third-party dependency: check whether the same effect can
  be achieved with the existing `[workspace.dependencies]` set. New
  dependencies require a one-line justification in the commit message.
- Splitting a crate: prefer a single crate with internal modules until
  the boundary is forced (separate compile unit needed, downstream wants
  to depend on one half only). Premature crate-splits raise compile times
  without payback.
- Adding a trait: only at the three known swap-points (DB driver, plugin
  host, secret store) or when a second concrete implementation is about
  to be written. Speculative traits are removed in review.
