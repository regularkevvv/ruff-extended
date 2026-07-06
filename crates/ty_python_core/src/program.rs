use crate::environment::InferenceSettings;
use crate::{Db, platform::PythonPlatform};

use ruff_db::{files::File, system::SystemPath};
use ruff_python_ast::PythonVersion;
use salsa::{Durability, Setter};
use ty_module_resolver::{ProgramFile, ResolverProgram, SearchPaths};
use ty_site_packages::{PythonVersionSource, PythonVersionWithSource};

// Re-export the misconfiguration strategy types from ty_module_resolver.
pub use ty_module_resolver::{FallibleStrategy, MisconfigurationStrategy, UseDefaultStrategy};

/// The semantic Python environment used to analyze source files.
#[salsa::input(heap_size = ruff_memory_usage::heap_size)]
#[derive(Debug)]
pub struct Program {
    resolver_program: ResolverProgram,

    #[returns(ref)]
    pub python_version_source: PythonVersionSource,

    #[returns(ref)]
    pub python_platform: PythonPlatform,

    #[returns(ref)]
    pub settings: InferenceSettings,
}

impl get_size2::GetSize for Program {}

impl Program {
    pub fn create(db: &dyn Db, settings: &ProgramSettings) -> Self {
        Self::from_settings(db, settings, &InferenceSettings::default())
    }

    pub fn from_settings(
        db: &dyn Db,
        settings: &ProgramSettings,
        inference_settings: &InferenceSettings,
    ) -> Self {
        let resolver_program =
            ResolverProgram::create(db, settings.python_version.version, &settings.search_paths);
        Self::builder(
            resolver_program,
            settings.python_version.source.clone(),
            settings.python_platform.clone(),
            inference_settings.clone(),
        )
        .durability(Durability::HIGH)
        .new(db)
    }

    #[must_use]
    pub fn with_inference_settings(self, db: &dyn Db, settings: InferenceSettings) -> Self {
        let resolver = ResolverProgram::create(db, self.python_version(db), self.search_paths(db));
        Self::builder(
            resolver,
            self.python_version_source(db).clone(),
            self.python_platform(db).clone(),
            settings,
        )
        .durability(Durability::HIGH)
        .new(db)
    }

    pub fn update_from_settings(self, db: &mut dyn Db, settings: ProgramSettings) -> bool {
        let ProgramSettings {
            python_version,
            python_platform,
            search_paths,
        } = settings;

        let resolver_changed = self
            .resolver(db)
            .update(db, python_version.version, search_paths);
        let source_changed = self.python_version_source(db) != &python_version.source;
        if source_changed {
            self.set_python_version_source(db).to(python_version.source);
        }
        let platform_changed = self.python_platform(db) != &python_platform;
        if platform_changed {
            self.set_python_platform(db).to(python_platform);
        }

        resolver_changed || source_changed || platform_changed
    }

    pub fn update_inference_settings(self, db: &mut dyn Db, settings: InferenceSettings) -> bool {
        let changed = self.settings(db) != &settings;
        if changed {
            self.set_settings(db).to(settings);
        }
        changed
    }

    pub fn freeze(self, db: &mut dyn Db) {
        self.resolver(db).freeze(db);

        let durability = Durability::NEVER_CHANGE;
        let source = self.python_version_source(db).clone();
        let platform = self.python_platform(db).clone();
        let settings = self.settings(db).clone();
        let resolver = self.resolver(db);

        self.set_resolver_program(db)
            .with_durability(durability)
            .to(resolver);
        self.set_python_version_source(db)
            .with_durability(durability)
            .to(source);
        self.set_python_platform(db)
            .with_durability(durability)
            .to(platform);
        self.set_settings(db)
            .with_durability(durability)
            .to(settings);
    }

    pub fn custom_stdlib_search_path(self, db: &dyn Db) -> Option<&SystemPath> {
        self.search_paths(db).custom_stdlib()
    }

    pub fn python_version(self, db: &dyn Db) -> PythonVersion {
        self.resolver(db).python_version(db)
    }

    pub fn search_paths(self, db: &dyn Db) -> &SearchPaths {
        self.resolver(db).search_paths(db)
    }

    /// Returns the module-resolution projection of this program.
    pub fn resolver(self, db: &dyn Db) -> ResolverProgram {
        self.resolver_program(db)
    }

    pub fn file(self, db: &dyn Db, file: File) -> ProgramFile<'_> {
        ProgramFile::new(db, self.resolver(db), file)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, get_size2::GetSize)]
pub struct ProgramSettings {
    pub python_version: PythonVersionWithSource,
    pub python_platform: PythonPlatform,
    pub search_paths: SearchPaths,
}

#[cfg(test)]
mod tests {
    use ruff_python_ast::PythonVersion;
    use ty_module_resolver::SearchPaths;
    use ty_site_packages::{PythonVersionSource, PythonVersionWithSource};

    use super::{Program, ProgramSettings};
    use crate::db::tests::TestDbBuilder;

    fn settings(db: &dyn crate::Db, version: PythonVersion) -> ProgramSettings {
        ProgramSettings {
            python_version: PythonVersionWithSource {
                version,
                source: PythonVersionSource::default(),
            },
            python_platform: crate::platform::PythonPlatform::default(),
            search_paths: SearchPaths::empty(db.vendored()),
        }
    }

    #[test]
    fn programs_are_independent_inputs() -> anyhow::Result<()> {
        let db = TestDbBuilder::new().build()?;
        let first_settings = settings(&db, PythonVersion::default());
        let first = Program::create(&db, &first_settings);

        let mut different_provenance = first_settings;
        different_provenance.python_version.source = PythonVersionSource::Cli;
        let second = Program::create(&db, &different_provenance);

        assert_ne!(first, second);
        assert_eq!(second.python_version_source(&db), &PythonVersionSource::Cli);
        Ok(())
    }

    #[test]
    fn changing_settings_creates_an_independent_program() -> anyhow::Result<()> {
        let db = TestDbBuilder::new().build()?;
        let original_settings = settings(&db, PythonVersion::default());
        let original = Program::create(&db, &original_settings);

        let mut changed_settings = original_settings;
        changed_settings.python_version = PythonVersionWithSource {
            version: PythonVersion::PY311,
            source: PythonVersionSource::Cli,
        };
        let changed = Program::create(&db, &changed_settings);

        assert_ne!(changed, original);
        assert_eq!(original.python_version(&db), PythonVersion::default());
        assert_eq!(changed.python_version(&db), PythonVersion::PY311);
        Ok(())
    }
}
