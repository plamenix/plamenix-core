# plamenix-core

Shared Rust crates for the [Plamenix](../plamenix/) Firebird IDE. This
repository is consumed by both the Tauri desktop edition
(`plamenix-desktop`) and the Fastify web edition (`plamenix-web`, via
NAPI bindings).

## Crates

| Crate | Status | Purpose |
|-------|--------|---------|
| `plamenix-types` | initial | Shared value types: session and tab IDs, connection configuration. |

Additional crates (`plamenix-db`, `plamenix-plugin-host`,
`plamenix-plugin-sdk`, `plamenix-secret`, `plamenix-schema`,
`plamenix-export`, `plamenix-config`) land in subsequent commits as
their first concrete use case arrives. New crates are not pre-scaffolded.

## Build

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

Or via `just`:

```sh
just build
just test
just lint
just fmt
```

## Toolchain

Pinned to Rust **1.95** stable via `rust-toolchain.toml`. The toolchain
auto-installs the matching `rustfmt` and `clippy` components.

## Licence

Dual-licensed under **MIT OR Apache-2.0**. See [`LICENSE-MIT`](./LICENSE-MIT)
and [`LICENSE-APACHE`](./LICENSE-APACHE).

## Contributing

See [`CONTRIBUTING.md`](./CONTRIBUTING.md) for build, test, and lint
specifics. Global workspace conventions live in the meta-workspace
[`plamenix/CONTRIBUTING.md`](../plamenix/CONTRIBUTING.md).
