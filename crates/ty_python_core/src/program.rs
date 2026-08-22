use crate::{Db, platform::PythonPlatform};

use ruff_db::files::File;
use ruff_db::system::SystemPath;
use ruff_db::vendored::VendoredFileSystem;
use ruff_python_ast::PythonVersion;
use salsa::Setter;
use ty_module_resolver::{ResolverEnvironment, SearchPaths};
use ty_site_packages::PythonVersionWithSource;

use crate::ProgramFile;

// Re-export the misconfiguration strategy types from ty_module_resolver.
pub use ty_module_resolver::{FallibleStrategy, MisconfigurationStrategy, UseDefaultStrategy};

#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct Program<'db> {
    #[returns(ref)]
    pub python_platform: PythonPlatform,

    #[returns(copy)]
    pub resolver_environment: ResolverEnvironment<'db>,
}

impl get_size2::GetSize for Program<'_> {}

impl<'db> Program<'db> {
    /// Creates a program from settings whose search roots have already been registered.
    pub fn from_settings(db: &'db dyn Db, settings: ProgramSettings) -> Self {
        let ProgramSettings {
            python_version,
            python_platform,
            search_paths,
            // Plugin configuration is registered separately, through `SemanticPlugins`: this
            // constructor runs inside a tracked query, which cannot create Salsa inputs.
            semantic_plugins: _,
        } = settings;

        let resolver_environment =
            ResolverEnvironment::new(db, python_version.version, &search_paths);
        Program::new(db, python_platform, resolver_environment)
    }

    pub fn python_version(self, db: &'db dyn Db) -> PythonVersion {
        self.resolver_environment(db).python_version(db)
    }

    pub fn search_paths(self, db: &'db dyn Db) -> &'db SearchPaths {
        self.resolver_environment(db).search_paths(db)
    }

    pub fn program_file(self, db: &'db dyn Db, file: File) -> ProgramFile<'db> {
        ProgramFile::new(db, file, self)
    }

    pub fn custom_stdlib_search_path(self, db: &'db dyn Db) -> Option<&'db SystemPath> {
        self.search_paths(db).custom_stdlib()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, get_size2::GetSize)]
pub struct ProgramSettings {
    pub python_version: PythonVersionWithSource,
    pub python_platform: PythonPlatform,
    pub search_paths: SearchPaths,
    pub semantic_plugins: SemanticPluginEnvironment,
}

/// The semantic plugin environment configured for the project.
///
/// This is a Salsa input of its own rather than a field on [`Program`]. Plugin configuration is
/// project-wide, so unlike a Python version it has no per-file meaning, and keeping it separate
/// means it stays reachable from `db` alone in the plugin machinery, which runs in contexts that
/// have no file, scope, or definition to resolve a `Program` from.
///
/// Being an input keeps the fingerprint part of the query graph: changing the configured plugins
/// invalidates every check that consulted them.
#[salsa::input(singleton, heap_size=ruff_memory_usage::heap_size)]
pub struct SemanticPlugins {
    #[returns(ref)]
    pub environment: SemanticPluginEnvironment,
}

impl SemanticPlugins {
    /// Registers `environment`, replacing any previously configured one.
    pub fn init_or_update(db: &mut dyn Db, environment: SemanticPluginEnvironment) {
        match Self::try_get(db) {
            Some(plugins) => {
                if plugins.environment(db) != &environment {
                    tracing::debug!(
                        "Updating semantic plugin environment: fingerprint {}",
                        environment.fingerprint()
                    );
                    plugins.set_environment(db).to(environment);
                }
            }
            None => {
                Self::new(db, environment);
            }
        }
    }

    /// Registers `environment` on a database that has none yet.
    pub fn init(db: &dyn Db, environment: SemanticPluginEnvironment) {
        if Self::try_get(db).is_none() {
            Self::new(db, environment);
        }
    }

    /// The configured environment, or an empty one on a database that never registered plugins.
    pub fn environment_or_empty(db: &dyn Db) -> &SemanticPluginEnvironment {
        static EMPTY: std::sync::LazyLock<SemanticPluginEnvironment> =
            std::sync::LazyLock::new(SemanticPluginEnvironment::default);

        Self::try_get(db).map_or(&EMPTY, |plugins| plugins.environment(db))
    }
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

    #[must_use]
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

    #[must_use]
    pub fn with_instance_member_on_subclass_claims(
        mut self,
        claims: impl Into<Box<[String]>>,
    ) -> Self {
        self.instance_member_on_subclass_claims = claims.into();
        self
    }

    #[must_use]
    pub fn with_mutation_claims(
        mut self,
        exact: impl Into<Box<[String]>>,
        subclasses: impl Into<Box<[String]>>,
    ) -> Self {
        self.mutation_class_claims = exact.into();
        self.mutation_subclass_claims = subclasses.into();
        self
    }

    #[must_use]
    pub fn with_settings_module_claims(
        mut self,
        settings_module_claims: impl Into<Box<[String]>>,
    ) -> Self {
        self.settings_module_claims = settings_module_claims.into();
        self
    }

    #[must_use]
    pub const fn with_project_index_enabled(mut self, project_index_enabled: bool) -> Self {
        self.project_index_enabled = project_index_enabled;
        self
    }

    #[must_use]
    pub fn with_config_json(mut self, config_json: impl Into<String>) -> Self {
        self.config_json = config_json.into();
        self
    }

    #[must_use]
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

impl ProgramSettings {
    pub fn empty(vendored: &VendoredFileSystem) -> Self {
        Self {
            python_version: PythonVersionWithSource::default(),
            python_platform: PythonPlatform::default(),
            search_paths: SearchPaths::empty(vendored),
            semantic_plugins: SemanticPluginEnvironment::default(),
        }
    }
}
