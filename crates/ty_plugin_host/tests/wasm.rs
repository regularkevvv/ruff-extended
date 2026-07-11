//! End-to-end tests for the wasmtime plugin backend.
//!
//! These run only when the production WASM runtime and test fixture are both enabled. The happy-path
//! tests drive a real SDK plugin compiled to `wasm32-unknown-unknown` by the crate's `build.rs`; the
//! failure-path tests use tiny hand-written modules so the runner's trap/timeout/limit handling is
//! exercised deterministically.

#![cfg(all(feature = "plugins-wasm", feature = "test-wasm-fixture"))]

use ty_plugin_examples::{MiniDjangoPlugin, minidjango};
use ty_plugin_host::{
    HostError, PluginEnvironment, PluginHost, PluginRunner, RuntimeError, WasmLimits, WasmRunner,
};
use ty_plugin_protocol::{
    ArgumentKind, ArgumentSummary, AssignedValueSummary, BuildProjectIndexRequest, CallRequest,
    CallValueSummary, ClassSummary, FieldSummary, LiteralValue, PluginManifest, PluginRequest,
    PluginResponse, ProjectContext, ReceiverSummary, SemanticContext, SymbolRef, SymbolSource,
    TypeExpr,
};
use ty_plugin_sdk::{ManifestBuilder, Plugin};

/// The real SDK Mini-Django plugin, compiled to WASM by `build.rs`.
const MINIDJANGO_PLUGIN_WASM: &[u8] = include_bytes!(env!("TY_PLUGIN_WASM_FIXTURE"));

fn context() -> SemanticContext {
    SemanticContext {
        module: "app".to_string(),
        file_path: "/project/app.py".to_string(),
        python_version: "3.13".to_string(),
        platform: "linux".to_string(),
        speculative: false,
    }
}

fn project_context() -> ProjectContext {
    ProjectContext {
        root: "/project".to_string(),
        python_version: "3.13".to_string(),
        platform: "linux".to_string(),
        config: Default::default(),
    }
}

fn positional_str(value: &str) -> ArgumentSummary {
    ArgumentSummary {
        name: None,
        kind: ArgumentKind::Positional,
        type_expr: Some(TypeExpr::annotation("str")),
        value: LiteralValue::Str {
            value: value.to_string(),
        },
        source: None,
    }
}

fn keyword_bool(name: &str, value: bool) -> ArgumentSummary {
    ArgumentSummary {
        name: Some(name.to_string()),
        kind: ArgumentKind::Keyword,
        type_expr: Some(TypeExpr::annotation("bool")),
        value: LiteralValue::Bool { value },
        source: None,
    }
}

fn class_arg(qualified_name: &str) -> ArgumentSummary {
    ArgumentSummary {
        name: None,
        kind: ArgumentKind::Positional,
        type_expr: Some(TypeExpr::annotation(qualified_name)),
        value: LiteralValue::ClassRef(SymbolRef {
            qualified_name: qualified_name.to_string(),
        }),
        source: None,
    }
}

fn call_field(name: &str, callee: &str, arguments: Vec<ArgumentSummary>) -> FieldSummary {
    FieldSummary {
        name: name.to_string(),
        annotation: None,
        assigned_value: Some(AssignedValueSummary::Call(CallValueSummary {
            callee: SymbolRef {
                qualified_name: callee.to_string(),
            },
            arguments,
            return_type: None,
        })),
        inferred_type: None,
        has_default: false,
        source: SymbolSource::default(),
    }
}

fn model_class(qualified_name: &str, fields: Vec<FieldSummary>) -> ClassSummary {
    ClassSummary {
        qualified_name: qualified_name.to_string(),
        bases: vec![TypeExpr::expression(minidjango::MODEL_BASE)],
        decorators: Vec::new(),
        metaclass: None,
        fields,
        nested_classes: Vec::new(),
        class_constants: Vec::new(),
        source: SymbolSource::default(),
    }
}

fn minidjango_project_index_request() -> PluginRequest {
    PluginRequest::BuildProjectIndex(BuildProjectIndexRequest {
        context: project_context(),
        classes: vec![
            model_class(
                "app.Author",
                vec![call_field(
                    "name",
                    "minidjango.CharField",
                    vec![ArgumentSummary {
                        name: Some("max_length".to_string()),
                        kind: ArgumentKind::Keyword,
                        type_expr: Some(TypeExpr::annotation("int")),
                        value: LiteralValue::Int { value: 100 },
                        source: None,
                    }],
                )],
            ),
            model_class(
                "app.Book",
                vec![
                    call_field(
                        "title",
                        "minidjango.CharField",
                        vec![ArgumentSummary {
                            name: Some("max_length".to_string()),
                            kind: ArgumentKind::Keyword,
                            type_expr: Some(TypeExpr::annotation("int")),
                            value: LiteralValue::Int { value: 200 },
                            source: None,
                        }],
                    ),
                    call_field(
                        "pages",
                        "minidjango.IntegerField",
                        vec![keyword_bool("null", true)],
                    ),
                    call_field(
                        "author",
                        "minidjango.ForeignKey",
                        vec![class_arg("app.Author")],
                    ),
                ],
            ),
        ],
        settings: Vec::new(),
        previous_index_fingerprint: None,
    })
}

fn values_list_request(project_index: serde_json::Value) -> PluginRequest {
    PluginRequest::AdjustCallReturn(CallRequest {
        context: context(),
        callee: TypeExpr::expression("minidjango.Manager.values_list"),
        receiver: Some(ReceiverSummary {
            type_expr: TypeExpr::annotation("minidjango.Manager[app.Book]"),
            nominal_class: Some(minidjango::MANAGER_BASE.to_string()),
            generic_arguments: vec![TypeExpr::annotation("app.Book")],
            plugin_metadata: Default::default(),
        }),
        arguments: vec![positional_str("title"), positional_str("pages")],
        existing_signature: None,
        default_return_type: None,
        project_index: Some(project_index),
    })
}

fn minidjango_plugin_host(limits: WasmLimits) -> PluginHost<WasmRunner> {
    let manifest = MiniDjangoPlugin.manifest();
    let plugin_id = manifest.id.clone();
    let environment =
        PluginEnvironment::from_manifests(vec![manifest]).expect("example manifest is valid");
    let runner = WasmRunner::new(limits)
        .expect("engine builds")
        .with_plugin(plugin_id, MINIDJANGO_PLUGIN_WASM)
        .expect("fixture module compiles");
    PluginHost::new(environment, runner)
}

#[test]
fn runs_example_plugin_compiled_to_wasm() {
    let host = minidjango_plugin_host(WasmLimits::default());

    let index_response = host
        .execute("example.minidjango", &minidjango_project_index_request())
        .expect("plugin executes");
    let PluginResponse::ProjectIndex(index) = index_response else {
        panic!("expected a project-index response, got {index_response:?}");
    };
    assert_eq!(
        index.plugin_index["models"]["app.Book"]["fields"]["pages"],
        "int | None"
    );

    let response = host
        .execute(
            "example.minidjango",
            &values_list_request(index.plugin_index.clone()),
        )
        .expect("plugin executes");
    let PluginResponse::CallReturnPatch(patch) = response else {
        panic!("expected a call-return patch, got {response:?}");
    };
    assert_eq!(
        patch.return_type.expression,
        "minidjango.QuerySet[app.Book, tuple[str, int | None]]"
    );
    assert!(patch.diagnostics.is_empty());
}

#[test]
fn manifest_request_round_trips_through_wasm() {
    let host = minidjango_plugin_host(WasmLimits::default());

    let response = host
        .execute("example.minidjango", &PluginRequest::Manifest)
        .expect("plugin executes");

    let PluginResponse::Manifest(manifest) = response else {
        panic!("expected a manifest, got {response:?}");
    };
    assert_eq!(manifest.id, "example.minidjango");
    assert!(manifest.capabilities.project_index);
    assert!(manifest.capabilities.call_return);
}

/// Build a single-plugin runner over a hand-written module so failure paths are deterministic.
fn wat_runner(plugin_id: &str, wat: &str, limits: WasmLimits) -> (PluginEnvironment, WasmRunner) {
    let manifest: PluginManifest = ManifestBuilder::new(plugin_id, "test", "0.0.0").build();
    let environment = PluginEnvironment::from_manifests(vec![manifest]).expect("manifest is valid");
    let runner = WasmRunner::new(limits)
        .expect("engine builds")
        .with_plugin(plugin_id, wat)
        .expect("module compiles");
    (environment, runner)
}

#[test]
fn plugin_crash_is_reported_as_a_trap() {
    // `ty_plugin_handle` immediately hits `unreachable`, i.e. the plugin crashes.
    const CRASHER: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "ty_plugin_alloc") (param i32) (result i32) i32.const 0)
          (func (export "ty_plugin_handle") (param i32 i32) (result i64) unreachable))
    "#;

    let (environment, runner) = wat_runner("crasher", CRASHER, WasmLimits::default());
    let plugin = environment.plugin("crasher").expect("plugin is loaded");

    let error = runner
        .execute(plugin, &PluginRequest::Manifest)
        .expect_err("a crashing plugin fails");

    assert!(
        matches!(error, RuntimeError::Trap(_)),
        "expected a trap, got {error:?}"
    );
    assert!(error.hint().contains("crashed"));
}

#[test]
fn plugin_timeout_is_reported() {
    // `ty_plugin_handle` loops forever, so it must exhaust its fuel budget.
    const LOOPER: &str = r#"
        (module
          (memory (export "memory") 1)
          (func (export "ty_plugin_alloc") (param i32) (result i32) i32.const 0)
          (func (export "ty_plugin_handle") (param i32 i32) (result i64)
            (loop $l br $l)
            i64.const 0))
    "#;

    let limits = WasmLimits {
        fuel: 100_000,
        ..WasmLimits::default()
    };
    let (environment, runner) = wat_runner("looper", LOOPER, limits);
    let plugin = environment.plugin("looper").expect("plugin is loaded");

    let error = runner
        .execute(plugin, &PluginRequest::Manifest)
        .expect_err("a looping plugin fails");

    assert!(
        matches!(error, RuntimeError::Timeout),
        "expected a timeout, got {error:?}"
    );
    assert!(error.hint().contains("budget"));
}

#[test]
fn oversize_response_is_rejected() {
    // The real plugin's manifest response is well over four bytes.
    let limits = WasmLimits {
        max_response_bytes: 4,
        ..WasmLimits::default()
    };
    let host = minidjango_plugin_host(limits);

    let error = host
        .execute("example.minidjango", &PluginRequest::Manifest)
        .expect_err("an oversize response is rejected");

    assert!(
        matches!(
            error,
            HostError::Runtime {
                source: RuntimeError::ResponseTooLarge,
                ..
            }
        ),
        "expected a response-too-large error, got {error:?}"
    );
}
