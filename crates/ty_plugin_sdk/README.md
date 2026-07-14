# ty_plugin_sdk

[![crates.io](https://img.shields.io/crates/v/ty_plugin_sdk.svg)](https://crates.io/crates/ty_plugin_sdk)
[![docs.rs](https://docs.rs/ty_plugin_sdk/badge.svg)](https://docs.rs/ty_plugin_sdk)

The authoring SDK for sandboxed [ty-extended](https://github.com/regularkevvv/ty-extended)
semantic extensions.

Use this crate to declare what an extension owns, implement typed semantic hooks, return
declarative patches, and export the implementation as a WebAssembly module. It re-exports all wire
types from `ty_plugin_protocol` as `ty_plugin_sdk::protocol`, so an extension normally has only one
ty dependency.

## Architecture Boundary

An extension does not link to `ty_python_semantic`, Salsa, AST ids, or checker-owned type objects.
The host sends a serialized request for a manifest claim; the extension returns a serialized patch
that the host validates and applies.

```text
ty semantic query -> typed protocol request -> extension hook -> declarative patch -> ty
```

The same `Plugin` implementation can be unit-tested natively and exported to WASM for production.

## Quick Start

Create a library crate that produces both a native Rust library and a WASM-compatible dynamic
library:

```toml
[package]
name = "my-ty-extension"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["rlib", "cdylib"]

[dependencies]
ty_plugin_sdk = "0.0.3"
```

Implement `Plugin`, claim the matching hook in the manifest, and export it:

```rust
use ty_plugin_sdk::protocol::{
    CallRequest, PluginManifest, PluginResponse, RuntimeSpec, TypeExpr, WasmRuntimeSpec,
};
use ty_plugin_sdk::{dsl, ManifestBuilder, Plugin};

#[derive(Default)]
struct MyExtension;

impl Plugin for MyExtension {
    fn manifest(&self) -> PluginManifest {
        ManifestBuilder::new("my-extension", "My extension", env!("CARGO_PKG_VERSION"))
            .ty_compatibility(">=0.59.0,<0.60.0")
            .runtime(RuntimeSpec::Wasm(WasmRuntimeSpec {
                artifact: "my_extension.wasm".to_string(),
                sha256: None,
            }))
            .claim_call_return("my_library.Field")
            .build()
    }

    fn adjust_call_return(&self, _request: &CallRequest) -> PluginResponse {
        dsl::call_return(TypeExpr::annotation("str"))
    }
}

ty_plugin_sdk::export_plugin!(MyExtension::default());
```

Every hook has a `PluginResponse::NoChange` default. An implementation only overrides the hooks it
uses.

## Build the WASM Module

```shell
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
```

The example produces
`target/wasm32-unknown-unknown/release/my_ty_extension.wasm`.

`export_plugin!` generates the `ty_plugin_alloc` and `ty_plugin_handle` exports expected by the
host. The ABI transports JSON through the module's linear memory; the extension has no WASI or
ambient host capabilities.

## Manifests and Claims

`ManifestBuilder` starts with protocol defaults and keeps capability flags synchronized with
claims. For example, `claim_call_return` adds a function claim and enables the `call-return`
capability.

Claims keep routing precise: ty invokes an extension only for the classes, functions, methods,
attributes, settings, or mutations it declared. Useful builder methods include:

- `claim_class_transform` and `claim_subclass_transform`;
- `claim_class_member`, `claim_instance_member`, and subclass member claims;
- `claim_call_signature` and `claim_call_return`, including method variants;
- `project_index`, settings claims, and cross-symbol contribution targets;
- `claim_mutations` and `claim_mutations_on_subclass`;
- `stub_overlay`, `config_schema`, and `default_config`.

Always set a narrow `ty_compatibility` range for a published extension. The protocol and SDK are
versioned independently from ty-extended.

## Hook Reference

| `Plugin` method           | Capability                | Typical response        |
| ------------------------- | ------------------------- | ----------------------- |
| `analyze_class`           | `class-transform`         | `ClassPatch`            |
| `resolve_class_member`    | `class-member`            | `MemberPatch`           |
| `resolve_instance_member` | `instance-member`         | `MemberPatch`           |
| `adjust_call_signature`   | `call-signature`          | `CallSignaturePatch`    |
| `adjust_call_return`      | `call-return`             | `CallReturnPatch`       |
| `build_project_index`     | `project-index`           | `ProjectIndexResponse`  |
| `additional_dependencies` | `additional-dependencies` | `Vec<PluginDependency>` |
| `validate_mutation`       | `mutation-validation`     | `MutationResponse`      |

The `dsl` module provides small constructors for fields, parameters, signatures, members, class
patches, and call responses. For shapes not covered by a helper, use the re-exported protocol types
directly.

## Type Expressions

Types cross the boundary as `TypeExpr`, never as checker internals:

```rust
use ty_plugin_sdk::protocol::TypeExpr;

let annotation = TypeExpr::annotation("list[str]");
let expression = TypeExpr::expression("my_library.Model");

assert_eq!(annotation.expression, "list[str]");
assert_eq!(expression.expression, "my_library.Model");
```

Choose annotation mode for type syntax, expression mode for runtime symbol expressions, and stub
mode for a complete generated stub declaration.

## Test an Extension

Test the same implementation at three levels:

1. Call hook methods directly in ordinary Rust unit tests.
1. Call `Plugin::handle` or `Plugin::handle_json` to verify dispatch and wire serialization.
1. Build `wasm32-unknown-unknown` and run the packaged artifact with ty-extended against a fixture
    Python project.

Native tests stay fast because `export_plugin!` expands to nothing outside `wasm32`.

## Package and Configure It

Ship the generated `.wasm` file with a JSON serialization of the same `PluginManifest`. A Python
library can place both files in its import package and name the manifest `ty-plugin.json`; users
then opt into installed-package discovery:

```toml
# ty.toml
[plugins]
auto-discover = true
```

For an explicitly managed artifact:

```toml
# ty.toml
[plugins]
enabled = true

[[plugins.plugin]]
id = "my-extension"
path = ".ty/plugins/my_extension.wasm"
runtime = "wasm"
manifest-path = ".ty/plugins/my-extension.plugin.json"
trusted = true
```

See the [ty-extended extension authoring
guide](https://github.com/regularkevvv/ty-extended/blob/main/docs/extension-authoring.md) for the
end-to-end packaging workflow and the [runtime
guide](https://github.com/regularkevvv/ty-extended/blob/main/docs/extension-runtime.md) for host
loading, sandboxing, and failure behavior.
