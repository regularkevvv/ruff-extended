//! Protocol version negotiation and forward-compatibility tests.

use ty_plugin_protocol::{
    CURRENT_PROTOCOL_VERSION, PluginManifest, PluginResponse, ProtocolCompatibility,
    ProtocolVersion, TypeExpr,
};

fn host() -> ProtocolVersion {
    ProtocolVersion { major: 0, minor: 3 }
}

#[test]
fn current_version_is_self_compatible() {
    assert_eq!(
        CURRENT_PROTOCOL_VERSION.negotiate(CURRENT_PROTOCOL_VERSION),
        ProtocolCompatibility::Compatible
    );
    assert!(ProtocolVersion::CURRENT.supports(ProtocolVersion::CURRENT));
}

#[test]
fn accepts_equal_and_older_minor() {
    // A plugin built for the host's exact minor, or an older minor, is compatible.
    assert_eq!(
        host().negotiate(ProtocolVersion { major: 0, minor: 3 }),
        ProtocolCompatibility::Compatible
    );
    assert_eq!(
        host().negotiate(ProtocolVersion { major: 0, minor: 1 }),
        ProtocolCompatibility::Compatible
    );
}

#[test]
fn rejects_newer_minor() {
    // A plugin requiring protocol features the host has not implemented yet is rejected.
    assert_eq!(
        host().negotiate(ProtocolVersion { major: 0, minor: 4 }),
        ProtocolCompatibility::MinorTooNew
    );
    assert!(!host().supports(ProtocolVersion { major: 0, minor: 4 }));
}

#[test]
fn rejects_different_major() {
    assert_eq!(
        host().negotiate(ProtocolVersion { major: 1, minor: 0 }),
        ProtocolCompatibility::MajorMismatch
    );
    // A newer major with an older-looking minor is still rejected on the major check first.
    assert_eq!(
        host().negotiate(ProtocolVersion { major: 1, minor: 0 }),
        ProtocolCompatibility::MajorMismatch
    );
}

#[test]
fn manifest_tolerates_unknown_forward_compatible_fields() {
    // A manifest emitted by a newer plugin toolchain carries fields this host does not know
    // about. Because the protocol structs do not use `deny_unknown_fields`, the host parses the
    // known subset and ignores the rest instead of failing to load the plugin.
    let json = r#"{
        "id": "future.plugin",
        "name": "Future plugin",
        "version": "9.9.9",
        "protocol-version": { "major": 0, "minor": 1 },
        "ty-compatibility": { "requirement": ">=0.0.0" },
        "runtime": { "kind": "mock" },
        "capabilities": { "class-transform": true, "telepathy": true },
        "unheard-of-top-level-field": [1, 2, 3]
    }"#;

    let manifest: PluginManifest = serde_json::from_str(json).expect("forward-compatible parse");
    assert_eq!(manifest.id, "future.plugin");
    assert!(manifest.capabilities.class_transform);
}

#[test]
fn response_tolerates_unknown_forward_compatible_fields() {
    let json = r#"{
        "kind": "call-return-patch",
        "return-type": { "expression": "int", "mode": "annotation" },
        "confidence": 0.9
    }"#;

    let response: PluginResponse = serde_json::from_str(json).expect("forward-compatible parse");
    let PluginResponse::CallReturnPatch(patch) = response else {
        panic!("expected a call-return patch");
    };
    assert_eq!(patch.return_type.expression, "int");
}

#[test]
fn type_expression_without_snapshot_remains_compatible() {
    let type_expr: TypeExpr =
        serde_json::from_str(r#"{"expression":"tuple[str, int]","mode":"annotation"}"#)
            .expect("protocol-v1 type expression");

    assert!(type_expr.snapshot.is_none());
}
