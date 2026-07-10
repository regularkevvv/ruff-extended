//! Stable data protocol for `ty` semantic extensions.
//!
//! This crate intentionally contains only serialized protocol types. It must
//! not depend on `ty_python_semantic`, Salsa, AST ids, file ids, or any other
//! checker internals.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 1 };

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    /// The protocol version this build of the host implements.
    pub const CURRENT: ProtocolVersion = CURRENT_PROTOCOL_VERSION;

    /// Negotiate whether a host advertising `self` can serve a plugin built for `plugin`.
    ///
    /// The rule mirrors pre-1.0 semver expectations: the `major` numbers must match, and the
    /// host must be at least as new as the plugin's `minor` (a plugin built against protocol
    /// features the host does not yet implement is rejected). Because the protocol structs do
    /// not set `deny_unknown_fields`, an older host can still *parse* a newer plugin's messages;
    /// negotiation is what prevents it from silently dropping features it cannot honor.
    #[must_use]
    pub fn negotiate(self, plugin: ProtocolVersion) -> ProtocolCompatibility {
        if self.major != plugin.major {
            ProtocolCompatibility::MajorMismatch
        } else if plugin.minor > self.minor {
            ProtocolCompatibility::MinorTooNew
        } else {
            ProtocolCompatibility::Compatible
        }
    }

    /// Convenience wrapper around [`ProtocolVersion::negotiate`] for callers that only need a
    /// boolean answer.
    #[must_use]
    pub fn supports(self, plugin: ProtocolVersion) -> bool {
        self.negotiate(plugin) == ProtocolCompatibility::Compatible
    }
}

/// The outcome of negotiating a plugin's declared [`ProtocolVersion`] against the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolCompatibility {
    /// The host can serve the plugin.
    Compatible,
    /// The `major` versions differ; the wire shape is not guaranteed to match.
    MajorMismatch,
    /// The plugin requires a newer `minor` than the host implements.
    MinorTooNew,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct VersionReq {
    pub requirement: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub protocol_version: ProtocolVersion,
    pub ty_compatibility: VersionReq,
    pub runtime: RuntimeSpec,
    #[serde(default)]
    pub capabilities: PluginCapabilities,
    #[serde(default)]
    pub claims: PluginClaims,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_config: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stub_overlays: Vec<StubOverlay>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RuntimeSpec {
    Mock,
    Wasm(WasmRuntimeSpec),
    Subprocess(SubprocessRuntimeSpec),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WasmRuntimeSpec {
    pub artifact: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SubprocessRuntimeSpec {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "Plugin capabilities are serialized manifest flags."
)]
pub struct PluginCapabilities {
    pub stub_overlays: bool,
    pub class_transform: bool,
    pub class_member: bool,
    pub instance_member: bool,
    pub call_signature: bool,
    pub call_return: bool,
    pub additional_dependencies: bool,
    pub project_index: bool,
    pub cross_symbol_contributions: bool,
    pub settings_data: bool,
    pub virtual_types: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct PluginClaims {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<ModuleClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<ClassClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<SymbolClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<SymbolClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<MethodClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<AttributeClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingsClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ModuleClaim {
    pub name: String,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ClassClaim {
    #[serde(flatten)]
    pub kind: ClassClaimKind,
}

impl ClassClaim {
    #[must_use]
    pub fn exact(qualified_name: impl Into<String>) -> Self {
        Self {
            kind: ClassClaimKind::Exact {
                qualified_name: qualified_name.into(),
            },
        }
    }

    #[must_use]
    pub fn subclass_of(base_qualified_name: impl Into<String>) -> Self {
        Self {
            kind: ClassClaimKind::SubclassOf {
                base_qualified_name: base_qualified_name.into(),
            },
        }
    }

    #[must_use]
    pub fn exact_qualified_name(&self) -> Option<&str> {
        match &self.kind {
            ClassClaimKind::Exact { qualified_name } => Some(qualified_name),
            ClassClaimKind::SubclassOf { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum ClassClaimKind {
    Exact { qualified_name: String },
    SubclassOf { base_qualified_name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SymbolClaim {
    pub qualified_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MethodClaim {
    #[serde(flatten)]
    pub kind: MethodClaimKind,
}

impl MethodClaim {
    #[must_use]
    pub fn exact(class_qualified_name: impl Into<String>, method_name: impl Into<String>) -> Self {
        Self {
            kind: MethodClaimKind::Exact {
                class_qualified_name: class_qualified_name.into(),
                method_name: method_name.into(),
            },
        }
    }

    #[must_use]
    pub fn on_subclass_of(
        base_qualified_name: impl Into<String>,
        method_name: impl Into<String>,
    ) -> Self {
        Self {
            kind: MethodClaimKind::OnSubclassOf {
                base_qualified_name: base_qualified_name.into(),
                method_name: method_name.into(),
            },
        }
    }

    #[must_use]
    pub fn exact_method(&self) -> Option<(&str, &str)> {
        match &self.kind {
            MethodClaimKind::Exact {
                class_qualified_name,
                method_name,
            } => Some((class_qualified_name, method_name)),
            MethodClaimKind::OnSubclassOf { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum MethodClaimKind {
    Exact {
        class_qualified_name: String,
        method_name: String,
    },
    OnSubclassOf {
        base_qualified_name: String,
        method_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AttributeClaim {
    #[serde(flatten)]
    pub kind: AttributeClaimKind,
}

impl AttributeClaim {
    #[must_use]
    pub fn exact(
        owner_qualified_name: impl Into<String>,
        attribute_name: impl Into<String>,
        scope: AttributeScope,
    ) -> Self {
        Self {
            kind: AttributeClaimKind::Exact {
                owner_qualified_name: owner_qualified_name.into(),
                attribute_name: attribute_name.into(),
                scope,
            },
        }
    }

    #[must_use]
    pub fn contribution_target(
        owner_base_qualified_name: impl Into<String>,
        scope: AttributeScope,
    ) -> Self {
        Self {
            kind: AttributeClaimKind::ContributionTarget {
                owner_base_qualified_name: owner_base_qualified_name.into(),
                scope,
            },
        }
    }

    #[must_use]
    pub fn exact_attribute(&self) -> Option<(&str, &str, AttributeScope)> {
        match &self.kind {
            AttributeClaimKind::Exact {
                owner_qualified_name,
                attribute_name,
                scope,
            } => Some((owner_qualified_name, attribute_name, *scope)),
            AttributeClaimKind::ContributionTarget { .. } => None,
        }
    }

    #[must_use]
    pub fn scope(&self) -> AttributeScope {
        match &self.kind {
            AttributeClaimKind::Exact { scope, .. }
            | AttributeClaimKind::ContributionTarget { scope, .. } => *scope,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum AttributeClaimKind {
    Exact {
        owner_qualified_name: String,
        attribute_name: String,
        scope: AttributeScope,
    },
    ContributionTarget {
        owner_base_qualified_name: String,
        scope: AttributeScope,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttributeScope {
    Class,
    Instance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SettingsClaim {
    pub module: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct StubOverlay {
    pub module: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PluginRequest {
    Manifest,
    BuildProjectIndex(BuildProjectIndexRequest),
    AnalyzeClass(AnalyzeClassRequest),
    ResolveClassMember(ResolveMemberRequest),
    ResolveInstanceMember(ResolveMemberRequest),
    AdjustCallSignature(CallRequest),
    AdjustCallReturn(CallRequest),
    AdditionalDependencies(DependencyRequest),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProjectContext {
    pub root: String,
    pub python_version: String,
    pub platform: String,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BuildProjectIndexRequest {
    pub context: ProjectContext,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<ClassSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingsModuleSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_index_fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AnalyzeClassRequest {
    pub context: SemanticContext,
    pub class: ClassSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_index: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ResolveMemberRequest {
    pub context: SemanticContext,
    pub owner: TypeExpr,
    pub member_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_member: Option<MemberSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_index: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CallRequest {
    pub context: SemanticContext,
    pub callee: TypeExpr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<ReceiverSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<ArgumentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_signature: Option<CallableSignature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_return_type: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_index: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DependencyRequest {
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SemanticContext {
    pub module: String,
    pub file_path: String,
    pub python_version: String,
    pub platform: String,
    #[serde(default)]
    pub speculative: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ClassSummary {
    pub qualified_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bases: Vec<TypeExpr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<CallOrSymbolSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metaclass: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nested_classes: Vec<NestedClassSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class_constants: Vec<ConstantSummary>,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SymbolRef {
    pub qualified_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum CallOrSymbolSummary {
    Symbol(SymbolRef),
    Call(CallValueSummary),
    Other {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inferred_type: Option<TypeExpr>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FieldSummary {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_value: Option<AssignedValueSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferred_type: Option<TypeExpr>,
    #[serde(default)]
    pub has_default: bool,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct NestedClassSummary {
    pub name: String,
    pub qualified_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bases: Vec<TypeExpr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class_constants: Vec<ConstantSummary>,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ConstantSummary {
    pub name: String,
    pub value: LiteralValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_expr: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum AssignedValueSummary {
    Call(CallValueSummary),
    Literal {
        value: LiteralValue,
    },
    Name(SymbolRef),
    Attribute(SymbolRef),
    Other {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inferred_type: Option<TypeExpr>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CallValueSummary {
    pub callee: SymbolRef,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<ArgumentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_type: Option<TypeExpr>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum LiteralValue {
    Bool { value: bool },
    Int { value: i64 },
    Str { value: String },
    None,
    EnumRef(SymbolRef),
    SymbolRef(SymbolRef),
    ClassRef(SymbolRef),
    Tuple { items: Vec<LiteralValue> },
    List { items: Vec<LiteralValue> },
    Dict { entries: Vec<LiteralDictEntry> },
    Unknown,
}

impl Default for LiteralValue {
    fn default() -> Self {
        Self::Unknown
    }
}

impl LiteralValue {
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LiteralDictEntry {
    pub key: LiteralValue,
    pub value: LiteralValue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MemberSummary {
    pub name: String,
    pub access: MemberAccessPatch,
    #[serde(default)]
    pub is_read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ArgumentSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: ArgumentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_expr: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "LiteralValue::is_unknown")]
    pub value: LiteralValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SymbolSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArgumentKind {
    Positional,
    Keyword,
    StarArgs,
    StarKwargs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ReceiverSummary {
    pub type_expr: TypeExpr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nominal_class: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generic_arguments: Vec<TypeExpr>,
    #[serde(default)]
    pub plugin_metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SettingsModuleSummary {
    pub module: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<SettingValueSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<PluginDependency>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SettingValueSummary {
    pub name: String,
    pub value: LiteralValue,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[expect(
    clippy::large_enum_variant,
    reason = "Protocol responses mirror the serialized wire shape."
)]
pub enum PluginResponse {
    Manifest(PluginManifest),
    ProjectIndex(ProjectIndexResponse),
    ClassPatch(ClassPatch),
    MemberPatch(MemberPatch),
    CallSignaturePatch(CallSignaturePatch),
    CallReturnPatch(CallReturnPatch),
    Dependencies(Vec<PluginDependency>),
    NoChange,
    Error(PluginError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProjectIndexResponse {
    #[serde(default)]
    pub plugin_index: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributions: Vec<Contribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub virtual_types: Vec<VirtualTypeDefinition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<PluginDependency>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ClassPatch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldPatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class_members: Vec<MemberPatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instance_members: Vec<MemberPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constructor: Option<CallableSignature>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FieldPatch {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<MemberAccessPatch>,
    pub instance_get_type: TypeExpr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_set_type: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constructor_parameter: Option<Parameter>,
    #[serde(default)]
    pub has_default: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MemberPatch {
    pub name: String,
    pub access: MemberAccessPatch,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum MemberAccessPatch {
    Value {
        type_expr: TypeExpr,
    },
    Descriptor {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        class_type: Option<TypeExpr>,
        instance_get_type: TypeExpr,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance_set_type: Option<TypeExpr>,
    },
}

impl MemberAccessPatch {
    #[must_use]
    pub fn value(type_expr: TypeExpr) -> Self {
        Self::Value { type_expr }
    }

    #[must_use]
    pub fn instance_get_type(&self) -> &TypeExpr {
        match self {
            Self::Value { type_expr }
            | Self::Descriptor {
                instance_get_type: type_expr,
                ..
            } => type_expr,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Contribution {
    pub source: SymbolSource,
    pub target: ContributionTarget,
    pub patch: ContributionPatch,
    pub conflict_key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum ContributionTarget {
    Class { qualified_name: String },
    Instance { qualified_name: String },
    Constructor { qualified_name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum ContributionPatch {
    Member(MemberPatch),
    Field(FieldPatch),
    Constructor(CallableSignature),
    Diagnostic(PluginDiagnostic),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CallSignaturePatch {
    pub signature: CallableSignature,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CallReturnPatch {
    pub return_type: TypeExpr,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CallableSignature {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    pub return_type: TypeExpr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Parameter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: ParameterKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_expr: Option<TypeExpr>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParameterKind {
    PositionalOnly,
    PositionalOrKeyword,
    VarArgs,
    KeywordOnly,
    Kwargs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TypeExpr {
    pub expression: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<ImportBinding>,
    pub mode: TypeExprMode,
}

impl TypeExpr {
    /// A bare type expression (e.g. a runtime value's inferred type), imports left empty.
    pub fn expression(expression: impl Into<String>) -> Self {
        Self::with_mode(expression, TypeExprMode::Expression)
    }

    /// An annotation-position type expression (e.g. a parameter or field annotation).
    pub fn annotation(expression: impl Into<String>) -> Self {
        Self::with_mode(expression, TypeExprMode::Annotation)
    }

    /// A stub-position type expression (e.g. content destined for a generated `.pyi` overlay).
    pub fn stub(expression: impl Into<String>) -> Self {
        Self::with_mode(expression, TypeExprMode::Stub)
    }

    fn with_mode(expression: impl Into<String>, mode: TypeExprMode) -> Self {
        Self {
            expression: expression.into(),
            imports: Vec::new(),
            mode,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ImportBinding {
    pub module: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TypeExprMode {
    Expression,
    Annotation,
    Stub,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct VirtualTypeDefinition {
    pub name: String,
    pub shape: VirtualTypeShape,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum VirtualTypeShape {
    Class {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        bases: Vec<TypeExpr>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        members: Vec<MemberPatch>,
    },
    TypedDict {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<VirtualTypeField>,
        #[serde(default = "default_true")]
        total: bool,
    },
    NamedTuple {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<VirtualTypeField>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct VirtualTypeField {
    pub name: String,
    pub type_expr: TypeExpr,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub read_only: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginDependency {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginDiagnostic {
    pub id: String,
    pub message: String,
    pub severity: DiagnosticSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<DiagnosticLocation>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DiagnosticLocation {
    pub file_path: String,
    pub start: TextPosition,
    pub end: TextPosition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SymbolSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<TextPosition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<TextPosition>,
}

impl Default for SymbolSource {
    fn default() -> Self {
        Self {
            module: None,
            qualified_name: None,
            file_path: None,
            start: None,
            end: None,
        }
    }
}

impl SymbolSource {
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TextPosition {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<PluginDiagnostic>,
}
