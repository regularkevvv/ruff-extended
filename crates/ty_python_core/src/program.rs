use crate::{Db, platform::PythonPlatform};

use ruff_db::system::SystemPath;
use ruff_python_ast::PythonVersion;
use salsa::Durability;
use salsa::Setter;
use ty_module_resolver::SearchPaths;
use ty_site_packages::PythonVersionWithSource;

// Re-export the misconfiguration strategy types from ty_module_resolver.
pub use ty_module_resolver::{FallibleStrategy, MisconfigurationStrategy, UseDefaultStrategy};

#[salsa::input(singleton, heap_size=ruff_memory_usage::heap_size)]
pub struct Program {
    #[returns(ref)]
    pub python_version_with_source: PythonVersionWithSource,

    #[returns(ref)]
    pub python_platform: PythonPlatform,

    #[returns(ref)]
    pub search_paths: SearchPaths,

    #[returns(ref)]
    pub semantic_plugins: SemanticPluginEnvironment,
}

impl Program {
    pub fn init_or_update(db: &mut dyn Db, settings: ProgramSettings) -> Self {
        match Self::try_get(db) {
            Some(program) => {
                program.update_from_settings(db, settings);
                program
            }
            None => Self::from_settings(db, settings),
        }
    }

    pub fn from_settings(db: &dyn Db, settings: ProgramSettings) -> Self {
        let ProgramSettings {
            python_version,
            python_platform,
            search_paths,
            semantic_plugins,
        } = settings;

        search_paths.try_register_static_roots(db);

        Program::builder(
            python_version,
            python_platform,
            search_paths,
            semantic_plugins,
        )
        .durability(Durability::HIGH)
        .new(db)
    }

    pub fn python_version(self, db: &dyn Db) -> PythonVersion {
        self.python_version_with_source(db).version
    }

    pub fn update_from_settings(self, db: &mut dyn Db, settings: ProgramSettings) {
        let ProgramSettings {
            python_version,
            python_platform,
            search_paths,
            semantic_plugins,
        } = settings;

        if self.search_paths(db) != &search_paths {
            tracing::debug!("Updating search paths");
            search_paths.try_register_static_roots(db);
            self.set_search_paths(db).to(search_paths);
        }

        if &python_platform != self.python_platform(db) {
            tracing::debug!("Updating python platform: `{python_platform:?}`");
            self.set_python_platform(db).to(python_platform);
        }

        if &python_version != self.python_version_with_source(db) {
            tracing::debug!(
                "Updating python version: Python {version}",
                version = python_version.version
            );
            self.set_python_version_with_source(db).to(python_version);
        }

        if self.semantic_plugins(db) != &semantic_plugins {
            tracing::debug!(
                "Updating semantic plugin environment: fingerprint {}",
                semantic_plugins.fingerprint()
            );
            self.set_semantic_plugins(db).to(semantic_plugins);
        }
    }

    /// Permanently freezes all program inputs.
    pub fn freeze(self, db: &mut dyn Db) {
        let durability = Durability::NEVER_CHANGE;
        let python_version = self.python_version_with_source(db).clone();
        let python_platform = self.python_platform(db).clone();
        let search_paths = self.search_paths(db).clone();
        let semantic_plugins = self.semantic_plugins(db).clone();

        self.set_python_version_with_source(db)
            .with_durability(durability)
            .to(python_version);
        self.set_python_platform(db)
            .with_durability(durability)
            .to(python_platform);
        self.set_search_paths(db)
            .with_durability(durability)
            .to(search_paths);
        self.set_semantic_plugins(db)
            .with_durability(durability)
            .to(semantic_plugins);
    }

    pub fn custom_stdlib_search_path(self, db: &dyn Db) -> Option<&SystemPath> {
        self.search_paths(db).custom_stdlib()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProgramSettings {
    pub python_version: PythonVersionWithSource,
    pub python_platform: PythonPlatform,
    pub search_paths: SearchPaths,
    pub semantic_plugins: SemanticPluginEnvironment,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, get_size2::GetSize)]
pub struct SemanticPluginEnvironment {
    fingerprint: u64,
    plugins: Box<[SemanticPlugin]>,
}

impl SemanticPluginEnvironment {
    pub fn new(fingerprint: u64, plugins: impl Into<Box<[SemanticPlugin]>>) -> Self {
        Self {
            fingerprint,
            plugins: plugins.into(),
        }
    }

    pub const fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    pub fn plugins(&self) -> &[SemanticPlugin] {
        &self.plugins
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, get_size2::GetSize)]
pub struct SemanticPlugin {
    id: String,
    runtime: SemanticPluginRuntime,
    class_transform_claims: Box<[String]>,
    class_member_claims: Box<[SemanticPluginMemberClaim]>,
    instance_member_claims: Box<[SemanticPluginMemberClaim]>,
    instance_member_on_subclass_claims: Box<[String]>,
    mutation_class_claims: Box<[String]>,
    mutation_subclass_claims: Box<[String]>,
    /// Qualified names of callees (functions, methods, or constructors keyed by class name)
    /// whose call signature this plugin adjusts.
    call_signature_claims: Box<[String]>,
    /// Qualified names of callees whose call return type this plugin adjusts.
    call_return_claims: Box<[String]>,
    project_index_enabled: bool,
    config_json: String,
    strict_settings: bool,
    settings_module_claims: Box<[String]>,
    call_signature_method_on_subclass_claims: Box<[SemanticPluginMethodClaim]>,
    call_return_method_on_subclass_claims: Box<[SemanticPluginMethodClaim]>,
}

impl SemanticPlugin {
    pub fn new(
        id: impl Into<String>,
        runtime: SemanticPluginRuntime,
        class_transform_claims: impl Into<Box<[String]>>,
        class_member_claims: impl Into<Box<[SemanticPluginMemberClaim]>>,
        instance_member_claims: impl Into<Box<[SemanticPluginMemberClaim]>>,
        call_signature_claims: impl Into<Box<[String]>>,
        call_return_claims: impl Into<Box<[String]>>,
    ) -> Self {
        Self {
            id: id.into(),
            runtime,
            class_transform_claims: class_transform_claims.into(),
            class_member_claims: class_member_claims.into(),
            instance_member_claims: instance_member_claims.into(),
            instance_member_on_subclass_claims: Box::new([]),
            mutation_class_claims: Box::new([]),
            mutation_subclass_claims: Box::new([]),
            call_signature_claims: call_signature_claims.into(),
            call_return_claims: call_return_claims.into(),
            project_index_enabled: false,
            config_json: "{}".to_string(),
            strict_settings: false,
            settings_module_claims: Box::new([]),
            call_signature_method_on_subclass_claims: Box::new([]),
            call_return_method_on_subclass_claims: Box::new([]),
        }
    }

    pub fn with_call_method_on_subclass_claims(
        mut self,
        call_signature_method_on_subclass_claims: impl Into<Box<[SemanticPluginMethodClaim]>>,
        call_return_method_on_subclass_claims: impl Into<Box<[SemanticPluginMethodClaim]>>,
    ) -> Self {
        self.call_signature_method_on_subclass_claims =
            call_signature_method_on_subclass_claims.into();
        self.call_return_method_on_subclass_claims = call_return_method_on_subclass_claims.into();
        self
    }

    pub fn with_instance_member_on_subclass_claims(
        mut self,
        claims: impl Into<Box<[String]>>,
    ) -> Self {
        self.instance_member_on_subclass_claims = claims.into();
        self
    }

    pub fn with_mutation_claims(
        mut self,
        exact: impl Into<Box<[String]>>,
        subclasses: impl Into<Box<[String]>>,
    ) -> Self {
        self.mutation_class_claims = exact.into();
        self.mutation_subclass_claims = subclasses.into();
        self
    }

    pub fn with_settings_module_claims(
        mut self,
        settings_module_claims: impl Into<Box<[String]>>,
    ) -> Self {
        self.settings_module_claims = settings_module_claims.into();
        self
    }

    pub const fn with_project_index_enabled(mut self, project_index_enabled: bool) -> Self {
        self.project_index_enabled = project_index_enabled;
        self
    }

    pub fn with_config_json(mut self, config_json: impl Into<String>) -> Self {
        self.config_json = config_json.into();
        self
    }

    pub const fn with_strict_settings(mut self, strict_settings: bool) -> Self {
        self.strict_settings = strict_settings;
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn runtime(&self) -> SemanticPluginRuntime {
        self.runtime
    }

    pub fn class_transform_claims(&self) -> &[String] {
        &self.class_transform_claims
    }

    pub fn class_member_claims(&self) -> &[SemanticPluginMemberClaim] {
        &self.class_member_claims
    }

    pub fn instance_member_claims(&self) -> &[SemanticPluginMemberClaim] {
        &self.instance_member_claims
    }

    pub fn instance_member_on_subclass_claims(&self) -> &[String] {
        &self.instance_member_on_subclass_claims
    }

    pub fn mutation_class_claims(&self) -> &[String] {
        &self.mutation_class_claims
    }

    pub fn mutation_subclass_claims(&self) -> &[String] {
        &self.mutation_subclass_claims
    }

    pub fn call_signature_claims(&self) -> &[String] {
        &self.call_signature_claims
    }

    pub fn call_return_claims(&self) -> &[String] {
        &self.call_return_claims
    }

    pub const fn project_index_enabled(&self) -> bool {
        self.project_index_enabled
    }

    pub const fn strict_settings(&self) -> bool {
        self.strict_settings
    }

    pub fn config_json(&self) -> &str {
        &self.config_json
    }

    pub fn settings_module_claims(&self) -> &[String] {
        &self.settings_module_claims
    }

    pub fn call_signature_method_on_subclass_claims(&self) -> &[SemanticPluginMethodClaim] {
        &self.call_signature_method_on_subclass_claims
    }

    pub fn call_return_method_on_subclass_claims(&self) -> &[SemanticPluginMethodClaim] {
        &self.call_return_method_on_subclass_claims
    }
}

#[derive(Clone, Debug, Eq, PartialEq, get_size2::GetSize)]
pub struct SemanticPluginMemberClaim {
    owner_qualified_name: String,
    member_name: String,
}

impl SemanticPluginMemberClaim {
    pub fn new(owner_qualified_name: impl Into<String>, member_name: impl Into<String>) -> Self {
        Self {
            owner_qualified_name: owner_qualified_name.into(),
            member_name: member_name.into(),
        }
    }

    pub fn owner_qualified_name(&self) -> &str {
        &self.owner_qualified_name
    }

    pub fn member_name(&self) -> &str {
        &self.member_name
    }
}

#[derive(Clone, Debug, Eq, PartialEq, get_size2::GetSize)]
pub struct SemanticPluginMethodClaim {
    base_qualified_name: String,
    method_name: String,
}

impl SemanticPluginMethodClaim {
    pub fn on_subclass_of(
        base_qualified_name: impl Into<String>,
        method_name: impl Into<String>,
    ) -> Self {
        Self {
            base_qualified_name: base_qualified_name.into(),
            method_name: method_name.into(),
        }
    }

    pub fn base_qualified_name(&self) -> &str {
        &self.base_qualified_name
    }

    pub fn method_name(&self) -> &str {
        &self.method_name
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, get_size2::GetSize)]
pub enum SemanticPluginRuntime {
    Mock,
    InProcess,
    Wasm,
}
