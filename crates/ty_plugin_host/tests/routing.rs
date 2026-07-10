use ty_plugin_host::{HookKind, MockRunner, PluginEnvironment, PluginHost};
use ty_plugin_protocol::{
    AttributeClaim, AttributeScope, ClassClaim, ClassPatch, FieldPatch, MethodClaim,
    PluginCapabilities, PluginClaims, PluginManifest, PluginRequest, PluginResponse,
    ProtocolVersion, RuntimeSpec, SettingsClaim, SymbolClaim, SymbolSource, TypeExpr, VersionReq,
};

#[test]
fn builds_capability_routes_from_manifest_claims() {
    let environment = PluginEnvironment::from_manifests(vec![manifest()]).unwrap();
    let routes = environment.routes();

    let expected = vec!["plugin.model".to_string()];

    assert_eq!(
        routes.class_transform_plugins("example.Model"),
        expected.as_slice()
    );
    assert_eq!(
        routes.instance_member_plugins("example.Model", "dynamic_field"),
        expected.as_slice()
    );
    assert_eq!(
        routes.call_return_plugins("example.Field"),
        expected.as_slice()
    );
    assert_eq!(
        routes.subclass_transform_plugins("django.Model"),
        expected.as_slice()
    );
    assert_eq!(
        routes.instance_contribution_target_plugins("django.Model"),
        expected.as_slice()
    );
    assert_eq!(
        routes.call_return_method_on_subclass_plugins("django.Manager", "filter"),
        expected.as_slice()
    );
    assert_eq!(routes.settings_plugins("app.settings"), expected.as_slice());
    assert_eq!(routes.project_index_plugins(), expected.as_slice());
    assert!(routes.class_transform_plugins("other.Model").is_empty());
    assert!(
        routes
            .instance_member_plugins("example.Model", "unknown")
            .is_empty()
    );
}

#[test]
fn mock_runner_returns_registered_response() {
    let environment = PluginEnvironment::from_manifests(vec![manifest()]).unwrap();
    let response = PluginResponse::ClassPatch(ClassPatch {
        fields: vec![FieldPatch {
            name: "name".to_string(),
            descriptor: None,
            instance_get_type: TypeExpr::expression("str"),
            instance_set_type: Some(TypeExpr::expression("str")),
            constructor_parameter: None,
            has_default: false,
        }],
        class_members: Vec::new(),
        instance_members: Vec::new(),
        constructor: None,
        diagnostics: Vec::new(),
    });

    let host = PluginHost::new(
        environment,
        MockRunner::default().with_response(
            "plugin.model",
            HookKind::AnalyzeClass,
            response.clone(),
        ),
    );

    assert_eq!(
        host.execute("plugin.model", &PluginRequest::Manifest)
            .unwrap(),
        PluginResponse::NoChange
    );

    assert_eq!(
        host.execute(
            "plugin.model",
            &PluginRequest::AnalyzeClass(ty_plugin_protocol::AnalyzeClassRequest {
                context: ty_plugin_protocol::SemanticContext {
                    module: "app".to_string(),
                    file_path: "/project/app.py".to_string(),
                    python_version: "3.13".to_string(),
                    platform: "linux".to_string(),
                    speculative: false,
                },
                class: ty_plugin_protocol::ClassSummary {
                    qualified_name: "app.User".to_string(),
                    bases: Vec::new(),
                    decorators: Vec::new(),
                    metaclass: None,
                    fields: Vec::new(),
                    nested_classes: Vec::new(),
                    class_constants: Vec::new(),
                    source: SymbolSource::default(),
                },
                project_index: None,
            })
        )
        .unwrap(),
        response
    );
}

#[test]
fn rejects_incompatible_protocol_major_version() {
    let mut manifest = manifest();
    manifest.protocol_version = ProtocolVersion {
        major: 99,
        minor: 0,
    };

    let error = PluginEnvironment::from_manifests(vec![manifest]).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("unsupported protocol version 99.0")
    );
}

#[test]
fn rejects_protocol_minor_newer_than_host() {
    // A plugin built against a newer protocol minor requires features this host does not
    // implement, so it must be rejected rather than loaded with those features silently dropped.
    let mut manifest = manifest();
    manifest.protocol_version = ProtocolVersion {
        major: ty_plugin_protocol::CURRENT_PROTOCOL_VERSION.major,
        minor: ty_plugin_protocol::CURRENT_PROTOCOL_VERSION.minor + 1,
    };

    let error = PluginEnvironment::from_manifests(vec![manifest]).unwrap_err();

    assert!(error.to_string().contains("unsupported protocol version"));
}

#[test]
fn rejects_contribution_target_claim_without_cross_symbol_capability() {
    let mut manifest = manifest();
    manifest.capabilities.cross_symbol_contributions = false;

    let error = PluginEnvironment::from_manifests(vec![manifest]).unwrap_err();

    assert!(
        error.to_string().contains(
            "contribution-target claims without the cross-symbol-contributions capability"
        )
    );
}

fn manifest() -> PluginManifest {
    PluginManifest {
        id: "plugin.model".to_string(),
        name: "Model plugin".to_string(),
        version: "0.1.0".to_string(),
        protocol_version: ProtocolVersion { major: 0, minor: 1 },
        ty_compatibility: VersionReq {
            requirement: ">=0.0.0".to_string(),
        },
        runtime: RuntimeSpec::Mock,
        capabilities: PluginCapabilities {
            class_transform: true,
            instance_member: true,
            call_return: true,
            project_index: true,
            cross_symbol_contributions: true,
            settings_data: true,
            ..PluginCapabilities::default()
        },
        claims: PluginClaims {
            classes: vec![
                ClassClaim::exact("example.Model"),
                ClassClaim::subclass_of("django.Model"),
            ],
            attributes: vec![
                AttributeClaim::exact("example.Model", "dynamic_field", AttributeScope::Instance),
                AttributeClaim::contribution_target("django.Model", AttributeScope::Instance),
            ],
            functions: vec![SymbolClaim {
                qualified_name: "example.Field".to_string(),
            }],
            methods: vec![MethodClaim::on_subclass_of("django.Manager", "filter")],
            settings: vec![SettingsClaim {
                module: "app.settings".to_string(),
            }],
            ..PluginClaims::default()
        },
        config_schema: None,
        default_config: None,
        stub_overlays: Vec::new(),
    }
}
