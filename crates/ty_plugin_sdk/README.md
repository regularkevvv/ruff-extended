# ty_plugin_sdk

Authoring SDK for `ty` semantic extensions.

This crate provides the extension manifest builder, hook trait, JSON dispatch helpers, and WASM
export macro used by extension authors. Raw wire types are re-exported from `ty_plugin_protocol`.

```toml
[dependencies]
ty_plugin_sdk = "0.0.1"
```

```rust
use ty_plugin_sdk::protocol::{CallRequest, PluginManifest, PluginResponse, TypeExpr};
use ty_plugin_sdk::{dsl, ManifestBuilder, Plugin};

#[derive(Default)]
pub struct MyExtension;

impl Plugin for MyExtension {
    fn manifest(&self) -> PluginManifest {
        ManifestBuilder::new("my-extension", "My ty extension", "0.1.0")
            .claim_call_return("my_framework.Field")
            .build()
    }

    fn adjust_call_return(&self, _request: &CallRequest) -> PluginResponse {
        dsl::call_return(TypeExpr::annotation("str"))
    }
}

ty_plugin_sdk::export_plugin!(MyExtension::default());
```

Build extensions as WASM artifacts:

```shell
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
```

The source lives in the `ruff` submodule of
[`regularkevvv/ty-extended`](https://github.com/regularkevvv/ty-extended), backed by
[`regularkevvv/ruff-extended`](https://github.com/regularkevvv/ruff-extended). See the
[extension authoring guide](https://github.com/regularkevvv/ty-extended/blob/main/docs/extension-authoring.md)
for manifests, hooks, project configuration, and packaging guidance.
