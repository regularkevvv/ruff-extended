# ty_plugin_protocol

[![crates.io](https://img.shields.io/crates/v/ty_plugin_protocol.svg)](https://crates.io/crates/ty_plugin_protocol)
[![docs.rs](https://docs.rs/ty_plugin_protocol/badge.svg)](https://docs.rs/ty_plugin_protocol)

The stable serialized contract between
[ty-extended](https://github.com/regularkevvv/ty-extended) and semantic extensions.

This crate intentionally contains data types only. It does not depend on checker internals, Salsa,
AST ids, the plugin host, or a WASM engine. Manifests, requests, responses, claims, patches,
diagnostics, and type expressions can therefore be serialized independently of ty's implementation.

Most extension authors should depend on
[`ty_plugin_sdk`](https://crates.io/crates/ty_plugin_sdk), which re-exports this crate as
`ty_plugin_sdk::protocol` and adds the `Plugin` trait, `ManifestBuilder`, typed patch helpers,
dispatch, and WASM exports.

Depend on `ty_plugin_protocol` directly when implementing a host, validating manifests, inspecting
wire messages, or building protocol tooling.

## Add the Dependency

```toml
[dependencies]
ty_plugin_protocol = "0.0.3"
```

## Protocol Model

The main types are:

- `PluginManifest`: identity, compatibility, runtime, capabilities, claims, configuration, and
    stub overlays;
- `PluginRequest`: the tagged request enum sent by a host;
- `PluginResponse`: the tagged response enum returned by an extension;
- request summaries such as `AnalyzeClassRequest`, `CallRequest`, and `ResolveMemberRequest`;
- declarative outputs such as `ClassPatch`, `MemberPatch`, `CallSignaturePatch`, and
    `ProjectIndexResponse`;
- `TypeExpr`: source-level type data with expression, annotation, or stub mode;
- `ProtocolVersion`: compatibility negotiation between a host and extension.

The wire format is JSON with kebab-case field and variant names. A request is self-contained and a
response is data; neither side shares memory objects from the checker.

## Compatibility Negotiation

The protocol is pre-1.0. A host accepts the same protocol major and any extension minor version no
newer than its own:

```rust
use ty_plugin_protocol::{ProtocolCompatibility, ProtocolVersion};

let host = ProtocolVersion { major: 0, minor: 3 };
let extension = ProtocolVersion { major: 0, minor: 2 };

assert_eq!(
    host.negotiate(extension),
    ProtocolCompatibility::Compatible,
);
```

Unknown JSON fields remain parseable for forward transport, but successful deserialization is not
permission to use unsupported behavior. Always negotiate the version before dispatching requests.

## Example Manifest Fragment

```json
{
  "id": "my-extension",
  "name": "My extension",
  "version": "0.1.0",
  "protocol-version": { "major": 0, "minor": 3 },
  "ty-compatibility": { "requirement": ">=0.59.0,<0.60.0" },
  "runtime": {
    "kind": "wasm",
    "artifact": "my_extension.wasm"
  },
  "capabilities": {
    "call-return": true
  },
  "claims": {
    "functions": [
      { "qualified-name": "my_library.Field" }
    ]
  }
}
```

Use `ty_plugin_sdk::ManifestBuilder` instead of hand-writing production manifests; it keeps claims
and capability flags aligned.

See the [extension authoring
guide](https://github.com/regularkevvv/ty-extended/blob/main/docs/extension-authoring.md) to build a
complete WASM extension and the [`ty_plugin_sdk` API documentation](https://docs.rs/ty_plugin_sdk)
for the author-facing interface.
