# plamenix-plugin-sdk

Authoring SDK for [Plamenix](https://plamenix.dev) plugins. Provides the
public surface external plugin authors compile against:

- The `plamenix:plugin` **WIT contract** as a bundled `&str` constant
  (`WIT_CONTRACT`) for tooling that doesn't want to read the on-disk
  file.
- The **subprocess activation protocol** helper — `subprocess::run`,
  `subprocess::Context`, `subprocess::Activation` — used by plugins
  that opt out of the WASM sandbox via
  `runtime.requires_subprocess = true` in their manifest.

## Quickstart

For step-by-step instructions that take you from `plamenix new` to a
signed, installed plugin, see the **[Write your first Plamenix
plugin](https://github.com/plamenix/plamenix/blob/main/docs/tutorial-first-plugin.md)**
tutorial in the meta-workspace.

This README is the docs.rs landing page; the API reference is the
crate-level rustdoc (`cargo doc -p plamenix-plugin-sdk --open`).

### WASM plugin (preferred path)

1. Add the SDK as a dependency:

   ```toml
   [dependencies]
   plamenix-plugin-sdk = "1.0.0-beta"  # or path / workspace dep
   wit-bindgen = "0.46"                # pin to the host's version
   ```

2. Copy the WIT contract into your plugin crate's `wit/` directory.
   The SDK exposes it as a `&str` if you want to avoid the on-disk
   duplicate:

   ```rust
   const PLAMENIX_WIT: &str = plamenix_plugin_sdk::WIT_CONTRACT;
   ```

3. Drive `wit-bindgen!`:

   ```rust
   wit_bindgen::generate!({
       path: "wit",
       world: "plamenix-plugin",
   });

   struct MyPlugin;

   impl exports::plamenix::plugin::plugin::Guest for MyPlugin {
       fn activate() -> exports::plamenix::plugin::plugin::Activation {
           exports::plamenix::plugin::plugin::Activation::Ok
       }
   }

   export!(MyPlugin);
   ```

4. Compile for `wasm32-wasip2`. The resulting `.wasm` is the
   `entry_points.wasm` member of your `.plx` bundle.

### Subprocess plugin

For plugins that need OS-level features the WASM sandbox doesn't
expose, the SDK ships a 1:1 helper for the host's activation
protocol. Add the SDK + write a normal `main`:

```rust
use plamenix_plugin_sdk::subprocess::{Activation, Context, log_info, run};

fn main() -> std::process::ExitCode {
    run(|ctx: &Context| {
        log_info(format!(
            "activating {} on host {}",
            ctx.plugin_id, ctx.host_version,
        ));
        Activation::Ok
    })
}
```

The manifest must declare `runtime.requires_subprocess = true` AND
include the `runtime.subprocess` capability in `[permissions].required`.
The host's manifest parser refuses the bundle if either gate is
missing.

## What ships in this crate

| Item | Module | Purpose |
|---|---|---|
| `WIT_CONTRACT` | `lib` | Verbatim WIT contract as a `&str`. |
| `Activation` | `subprocess` | Activation outcome (`Ok` / `Failed(String)`). |
| `Context` | `subprocess` | Args the host forwards on argv. |
| `run` | `subprocess` | Entry point for subprocess plugins. |
| `log_info` / `log_warn` / `log_error` | `subprocess` | Stderr diagnostic helpers. |

## What does NOT ship here

The parent-side runtime — bundle loader, manifest parser, wasmtime
engine, supervisor, instance registry, signature verifier — lives in
[`plamenix-plugin-host`](https://crates.io/crates/plamenix-plugin-host).
Plugin authors should NOT depend on the host crate; this SDK is the
stable surface across host revisions.

## Versioning

The SDK tracks the host's WIT contract version. Bumps mean ABI
changes. Pin `wit-bindgen` to the version listed in the workspace
`Cargo.toml` (currently `0.46`) to avoid silent drift.

## License

Dual-licensed under MIT or Apache-2.0. See the workspace root for
the full license text.
