# Contributing to plamenix-core

This file covers concerns specific to the shared Rust crates. Global
project conventions (branching, commit style, licence, code of conduct)
live in the meta-workspace
[`plamenix/CONTRIBUTING.md`](../plamenix/CONTRIBUTING.md).

## Prerequisites

- Rust **1.95** stable. The `rust-toolchain.toml` file pins the compiler;
  `rustup` installs it automatically on the first build.
- [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) and
  [`cargo-msrv`](https://github.com/foresterre/cargo-msrv) (optional, for
  dependency audits and MSRV verification).
- [`just`](https://github.com/casey/just) command runner.

## Build, test, lint

```sh
just build      # cargo build --workspace
just test       # cargo test --workspace
just fmt        # cargo fmt --all
just fmt-check  # cargo fmt --all -- --check
just lint       # cargo clippy --workspace --all-targets -- -D warnings
just doc        # cargo doc --workspace --no-deps --open
just all        # fmt-check + lint + test + doc
```

CI runs `just all`. Open a PR with all checks green.

## Adding a new crate

1. Create `crates/<name>/Cargo.toml` and `crates/<name>/src/lib.rs`.
2. Set `version`, `edition`, `rust-version`, `license`, and `authors` to
   `workspace = true`.
3. Add `[lints] workspace = true` so workspace-wide lint configuration
   applies.
4. Open `lib.rs` with a `//!` crate-level doc block explaining purpose,
   mental model, and how to extend.
5. The workspace's `members = ["crates/*"]` glob picks up the new crate
   automatically.

## Adding a new dependency

- Pin the major version in the workspace `[workspace.dependencies]`
  block.
- Reference it from member crates as `dep_name = { workspace = true }`.
- The commit that introduces the dependency must explain why in one
  short line. Dependencies are liabilities; review them like code.
- Run `cargo deny check` (optional but encouraged) to catch licence or
  vulnerability issues.

## Testing

- Unit tests live next to the code they cover, inside
  `#[cfg(test)] mod tests { ... }` at the bottom of the file.
- Integration tests live in the crate's `tests/` directory (one file =
  one binary). Use these to exercise the crate as an external consumer.
- Doctests are part of the public-API documentation contract. Every
  non-trivial public function gets an `# Examples` block that compiles
  and runs under `cargo test --doc`.
- Property-based tests live in the same `#[cfg(test)]` modules and use
  `proptest`. Reach for them when the input space is large (SQL
  escaping, filter logic, type coercions).

## Commits

[Conventional Commits](https://www.conventionalcommits.org). Subject
line under 50 characters. Body wrapped at 72 columns when present.
Example:

```
feat(types): add ConnectionConfig encryption fields

Adds optional encryption_key, fbclient_path, and encryption_required
fields to ConnectionConfig in preparation for the encrypted-connection
flow in M1.
```

## Licence

By contributing you agree your changes are dual-licensed under
**MIT OR Apache-2.0**.
