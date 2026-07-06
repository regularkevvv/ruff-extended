use ruff_db::files::File;
use ruff_python_ast::PythonVersion;
use salsa::{Durability, Setter};

use crate::{Db, SearchPaths};

/// The portion of a Python program used during parsing and module resolution.
#[salsa::input(heap_size = ruff_memory_usage::heap_size)]
#[derive(Debug)]
pub struct ResolverProgram {
    pub python_version: PythonVersion,

    #[returns(ref)]
    pub search_paths: SearchPaths,
}

impl get_size2::GetSize for ResolverProgram {}

impl ResolverProgram {
    pub fn create(db: &dyn Db, python_version: PythonVersion, search_paths: &SearchPaths) -> Self {
        search_paths.try_register_static_roots(db);
        Self::builder(python_version, search_paths.clone())
            .durability(Durability::HIGH)
            .new(db)
    }

    pub fn update(
        self,
        db: &mut dyn Db,
        python_version: PythonVersion,
        search_paths: SearchPaths,
    ) -> bool {
        let search_paths_changed = self.search_paths(db) != &search_paths;
        if search_paths_changed {
            search_paths.try_register_static_roots(db);
            self.set_search_paths(db).to(search_paths);
        }
        let python_version_changed = self.python_version(db) != python_version;
        if python_version_changed {
            self.set_python_version(db).to(python_version);
        }

        search_paths_changed || python_version_changed
    }

    pub fn freeze(self, db: &mut dyn Db) {
        let durability = Durability::NEVER_CHANGE;
        let python_version = self.python_version(db);
        let search_paths = self.search_paths(db).clone();

        self.set_python_version(db)
            .with_durability(durability)
            .to(python_version);
        self.set_search_paths(db)
            .with_durability(durability)
            .to(search_paths);
    }
}

/// A physical file interpreted in one module-resolution environment.
#[salsa::interned(debug, heap_size = ruff_memory_usage::heap_size)]
pub struct ProgramFile<'db> {
    pub program: ResolverProgram,
    pub file: File,
}
