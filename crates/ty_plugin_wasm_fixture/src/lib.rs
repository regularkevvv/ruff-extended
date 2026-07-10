//! A `cdylib` plugin artifact used as `ty_plugin_host`'s WASM end-to-end test fixture.
//!
//! It exports the [`MiniDjangoPlugin`](ty_plugin_examples::MiniDjangoPlugin) example
//! through the SDK's `wasm` ABI, so building this crate for `wasm32-unknown-unknown` produces a
//! real plugin the host's `WasmRunner` can load and drive. `ty_plugin_host`'s `build.rs` compiles
//! it under the `plugins-wasm` feature and hands the resulting `.wasm` bytes to the runner.
//!
//! On the host target `export_plugin!` expands to nothing; the `as _` imports below keep both
//! dependencies referenced so the crate builds cleanly for every target.

#[cfg(not(target_arch = "wasm32"))]
use ty_plugin_examples as _;
#[cfg(not(target_arch = "wasm32"))]
use ty_plugin_sdk as _;

ty_plugin_sdk::export_plugin!(ty_plugin_examples::MiniDjangoPlugin);
