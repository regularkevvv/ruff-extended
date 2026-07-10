# ty_plugin_protocol

Stable serialized protocol types for `ty` semantic extensions.

This crate intentionally contains only wire/data types. It does not depend on checker internals,
Salsa, AST ids, or `ty_plugin_host`.

Most extension authors should depend on [`ty_plugin_sdk`](https://crates.io/crates/ty_plugin_sdk)
instead. The SDK re-exports this crate as `ty_plugin_sdk::protocol` and provides the manifest
builder, hook trait, dispatch helpers, and WASM export macro.

Use this crate directly only when you are implementing a host, writing protocol tests, or building
tooling that needs to read or validate extension manifests and request/response payloads.

The source lives in the `ruff` submodule of
[`regularkevvv/ty-extended`](https://github.com/regularkevvv/ty-extended), backed by
[`regularkevvv/ruff-extended`](https://github.com/regularkevvv/ruff-extended). Start with the
[extension authoring guide](https://github.com/regularkevvv/ty-extended/blob/main/docs/extension-authoring.md)
for a complete extension crate layout.
