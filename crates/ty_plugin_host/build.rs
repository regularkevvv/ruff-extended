//! Builds the WASM plugin test fixture only when the `test-wasm-fixture` feature is enabled.
//!
//! This compiles `ty_plugin_wasm_fixture` for `wasm32-unknown-unknown` into a private target
//! directory and exposes the artifact path to the integration tests through the
//! `TY_PLUGIN_WASM_FIXTURE` environment variable. Production builds only need `plugins-wasm` and
//! therefore do not need the extra Rust target or pay for the fixture build.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    if std::env::var_os("CARGO_FEATURE_TEST_WASM_FIXTURE").is_none() {
        return;
    }

    // Rebuild the fixture when its sources or those of the crates it links change.
    for path in [
        "../ty_plugin_wasm_fixture/src/lib.rs",
        "../ty_plugin_wasm_fixture/Cargo.toml",
        "../ty_plugin_sdk/src",
        "../ty_plugin_examples/src",
        "../ty_plugin_protocol/src",
    ] {
        println!("cargo::rerun-if-changed={path}");
    }

    let out_dir =
        PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set for build scripts"));
    let target_dir = out_dir.join("wasm-fixture");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());

    let status = Command::new(&cargo)
        .args([
            "build",
            "--package",
            "ty_plugin_wasm_fixture",
            "--target",
            "wasm32-unknown-unknown",
            "--release",
        ])
        .arg("--target-dir")
        .arg(&target_dir)
        // Build the fixture hermetically. Without this, a parent `cargo clippy` leaks its
        // `clippy-driver` wrapper and `-D warnings` into this nested build, which then evaluates the
        // protocol crate's clippy `#[expect(...)]` against `wasm32` (where it does not fire) and
        // fails. The fixture is a plain artifact, so plain rustc with default flags is correct.
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .status()
        .expect("failed to spawn cargo to build the wasm plugin fixture");

    assert!(
        status.success(),
        "building the wasm plugin fixture failed; install the target with \
         `rustup target add wasm32-unknown-unknown`",
    );

    let artifact = target_dir
        .join("wasm32-unknown-unknown")
        .join("release")
        .join("ty_plugin_wasm_fixture.wasm");
    assert!(
        artifact.is_file(),
        "expected the wasm fixture at {}",
        artifact.display()
    );

    println!(
        "cargo::rustc-env=TY_PLUGIN_WASM_FIXTURE={}",
        artifact.display()
    );
}
