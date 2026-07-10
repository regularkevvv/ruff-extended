//! A fluent builder for [`PluginManifest`].

use ty_plugin_protocol::{
    AttributeClaim, AttributeScope, ClassClaim, MethodClaim, PluginCapabilities, PluginClaims,
    PluginManifest, ProtocolVersion, RuntimeSpec, SettingsClaim, StubOverlay, SymbolClaim,
    VersionReq,
};

/// Builds a [`PluginManifest`], filling in protocol defaults and keeping capability flags in sync
/// with the claims that require them.
///
/// Each `claim_*`/`stub_overlay` method both records the claim and enables the capability it
/// depends on, so an author cannot forget to flip the matching flag (which the host would
/// otherwise reject, e.g. stub overlays without the `stub-overlays` capability).
#[derive(Debug, Clone)]
pub struct ManifestBuilder {
    manifest: PluginManifest,
}

impl ManifestBuilder {
    /// Start a manifest with the given identity. The protocol version defaults to
    /// [`ProtocolVersion::CURRENT`], the runtime to [`RuntimeSpec::Mock`], and `ty`-compatibility
    /// to `>=0.0.0`; all capabilities start disabled.
    pub fn new(id: impl Into<String>, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            manifest: PluginManifest {
                id: id.into(),
                name: name.into(),
                version: version.into(),
                protocol_version: ProtocolVersion::CURRENT,
                ty_compatibility: VersionReq {
                    requirement: ">=0.0.0".to_string(),
                },
                runtime: RuntimeSpec::Mock,
                capabilities: PluginCapabilities::default(),
                claims: PluginClaims::default(),
                config_schema: None,
                default_config: None,
                stub_overlays: Vec::new(),
            },
        }
    }

    /// Override the declared protocol version (defaults to [`ProtocolVersion::CURRENT`]).
    #[must_use]
    pub fn protocol_version(mut self, version: ProtocolVersion) -> Self {
        self.manifest.protocol_version = version;
        self
    }

    /// Set the `ty`-compatibility requirement string (defaults to `>=0.0.0`).
    #[must_use]
    pub fn ty_compatibility(mut self, requirement: impl Into<String>) -> Self {
        self.manifest.ty_compatibility = VersionReq {
            requirement: requirement.into(),
        };
        self
    }

    /// Set the runtime the host should use to execute this plugin (defaults to
    /// [`RuntimeSpec::Mock`]).
    #[must_use]
    pub fn runtime(mut self, runtime: RuntimeSpec) -> Self {
        self.manifest.runtime = runtime;
        self
    }

    /// Claim a class for the `class-transform` hook and enable the capability.
    #[must_use]
    pub fn claim_class_transform(mut self, qualified_name: impl Into<String>) -> Self {
        self.manifest.capabilities.class_transform = true;
        self.manifest
            .claims
            .classes
            .push(ClassClaim::exact(qualified_name));
        self
    }

    /// Claim all subclasses of a base class for the `class-transform` hook.
    #[must_use]
    pub fn claim_subclass_transform(mut self, base_qualified_name: impl Into<String>) -> Self {
        self.manifest.capabilities.class_transform = true;
        self.manifest
            .claims
            .classes
            .push(ClassClaim::subclass_of(base_qualified_name));
        self
    }

    /// Claim a class-scope attribute for the `class-member` hook and enable the capability.
    #[must_use]
    pub fn claim_class_member(
        mut self,
        owner_qualified_name: impl Into<String>,
        attribute_name: impl Into<String>,
    ) -> Self {
        self.manifest.capabilities.class_member = true;
        self.manifest.claims.attributes.push(AttributeClaim::exact(
            owner_qualified_name,
            attribute_name,
            AttributeScope::Class,
        ));
        self
    }

    /// Claim an instance-scope attribute for the `instance-member` hook and enable the capability.
    #[must_use]
    pub fn claim_instance_member(
        mut self,
        owner_qualified_name: impl Into<String>,
        attribute_name: impl Into<String>,
    ) -> Self {
        self.manifest.capabilities.instance_member = true;
        self.manifest.claims.attributes.push(AttributeClaim::exact(
            owner_qualified_name,
            attribute_name,
            AttributeScope::Instance,
        ));
        self
    }

    /// Claim class-scope contributions to subclasses of a base class.
    #[must_use]
    pub fn claim_class_contribution_target(
        mut self,
        owner_base_qualified_name: impl Into<String>,
    ) -> Self {
        self.manifest.capabilities.project_index = true;
        self.manifest.capabilities.cross_symbol_contributions = true;
        self.manifest
            .claims
            .attributes
            .push(AttributeClaim::contribution_target(
                owner_base_qualified_name,
                AttributeScope::Class,
            ));
        self
    }

    /// Claim instance-scope contributions to subclasses of a base class.
    #[must_use]
    pub fn claim_instance_contribution_target(
        mut self,
        owner_base_qualified_name: impl Into<String>,
    ) -> Self {
        self.manifest.capabilities.project_index = true;
        self.manifest.capabilities.cross_symbol_contributions = true;
        self.manifest
            .claims
            .attributes
            .push(AttributeClaim::contribution_target(
                owner_base_qualified_name,
                AttributeScope::Instance,
            ));
        self
    }

    /// Claim a free function (or a constructor, named by its class) for the `call-signature` hook.
    #[must_use]
    pub fn claim_call_signature(mut self, qualified_name: impl Into<String>) -> Self {
        self.manifest.capabilities.call_signature = true;
        self.manifest.claims.functions.push(SymbolClaim {
            qualified_name: qualified_name.into(),
        });
        self
    }

    /// Claim a free function (or a constructor, named by its class) for the `call-return` hook.
    #[must_use]
    pub fn claim_call_return(mut self, qualified_name: impl Into<String>) -> Self {
        self.manifest.capabilities.call_return = true;
        self.manifest.claims.functions.push(SymbolClaim {
            qualified_name: qualified_name.into(),
        });
        self
    }

    /// Claim a method for the `call-signature` hook and enable the capability.
    #[must_use]
    pub fn claim_call_signature_method(
        mut self,
        class_qualified_name: impl Into<String>,
        method_name: impl Into<String>,
    ) -> Self {
        self.manifest.capabilities.call_signature = true;
        self.manifest
            .claims
            .methods
            .push(MethodClaim::exact(class_qualified_name, method_name));
        self
    }

    /// Claim method calls on subclasses of a base class for the `call-signature` hook.
    #[must_use]
    pub fn claim_call_signature_method_on_subclass(
        mut self,
        base_qualified_name: impl Into<String>,
        method_name: impl Into<String>,
    ) -> Self {
        self.manifest.capabilities.call_signature = true;
        self.manifest
            .claims
            .methods
            .push(MethodClaim::on_subclass_of(
                base_qualified_name,
                method_name,
            ));
        self
    }

    /// Claim a method for the `call-return` hook and enable the capability.
    #[must_use]
    pub fn claim_call_return_method(
        mut self,
        class_qualified_name: impl Into<String>,
        method_name: impl Into<String>,
    ) -> Self {
        self.manifest.capabilities.call_return = true;
        self.manifest
            .claims
            .methods
            .push(MethodClaim::exact(class_qualified_name, method_name));
        self
    }

    /// Claim method calls on subclasses of a base class for the `call-return` hook.
    #[must_use]
    pub fn claim_call_return_method_on_subclass(
        mut self,
        base_qualified_name: impl Into<String>,
        method_name: impl Into<String>,
    ) -> Self {
        self.manifest.capabilities.call_return = true;
        self.manifest
            .claims
            .methods
            .push(MethodClaim::on_subclass_of(
                base_qualified_name,
                method_name,
            ));
        self
    }

    /// Enable project indexing for this plugin.
    #[must_use]
    pub fn project_index(mut self) -> Self {
        self.manifest.capabilities.project_index = true;
        self
    }

    /// Declare a settings module the host should summarize for this plugin.
    #[must_use]
    pub fn settings_module(mut self, module: impl Into<String>) -> Self {
        self.manifest.capabilities.settings_data = true;
        self.manifest.claims.settings.push(SettingsClaim {
            module: module.into(),
        });
        self
    }

    /// Declare that this plugin may return host-owned virtual type definitions.
    #[must_use]
    pub fn virtual_types(mut self) -> Self {
        self.manifest.capabilities.virtual_types = true;
        self
    }

    /// Declare a `.pyi` stub overlay for a module and enable the `stub-overlays` capability.
    #[must_use]
    pub fn stub_overlay(mut self, module: impl Into<String>, path: impl Into<String>) -> Self {
        self.manifest.capabilities.stub_overlays = true;
        self.manifest.stub_overlays.push(StubOverlay {
            module: module.into(),
            path: path.into(),
            sha256: None,
        });
        self
    }

    /// Attach a JSON schema describing the plugin's accepted configuration.
    #[must_use]
    pub fn config_schema(mut self, schema: serde_json::Value) -> Self {
        self.manifest.config_schema = Some(schema);
        self
    }

    /// Attach the plugin's default configuration values.
    #[must_use]
    pub fn default_config(mut self, config: serde_json::Value) -> Self {
        self.manifest.default_config = Some(config);
        self
    }

    /// Finish building and return the [`PluginManifest`].
    #[must_use]
    pub fn build(self) -> PluginManifest {
        self.manifest
    }
}
