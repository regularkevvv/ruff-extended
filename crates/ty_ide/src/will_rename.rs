//! Import and reference edits for renaming Python modules and packages.

use crate::references::{contains_identifier, module_references_for_file};
use rayon::prelude::*;
use ruff_db::files::{File, system_path_to_file};
use ruff_db::source::source_text;
use ruff_db::system::{SystemPath, SystemPathBuf};
use ruff_python_ast::{
    self as ast, AnyNodeRef,
    visitor::source_order::{SourceOrderVisitor, TraversalSignal},
};
use ruff_text_size::TextRange;
use ty_module_resolver::{
    Module, ModuleName, ModuleResolveMode, file_to_module, is_legacy_namespace_package,
    resolve_module_confident, resolve_real_module_confident, search_paths,
};
use ty_project::Db;
use ty_python_semantic::ImportAliasResolution::PreserveAliases;
use ty_python_semantic::types::Type;
use ty_python_semantic::{HasType, ResolvedDefinition, SemanticModel, definitions_for_name};

/// Returns the text replacements to apply before renaming Python modules or regular packages.
///
/// Files can move between import roots; regular packages must keep the same logical parent. Imports
/// and semantic references are rewritten while meaningful aliases remain stable. Unsupported
/// input rejects the batch. Candidate files are analyzed concurrently, once per file.
pub fn will_rename_paths(
    db: &dyn Db,
    renames: &[PathRename],
    files: impl IntoIterator<Item = File>,
    file_is_in_scope: impl Fn(File) -> bool,
) -> Vec<FileRenameEdit> {
    let Some(plan) = RenamePlan::new(db, renames) else {
        return Vec::new();
    };

    let mut files: Vec<_> = files
        .into_iter()
        .filter(|file| file_is_in_scope(*file))
        .collect();
    for file in plan.rules.iter().filter_map(|rule| match rule.source {
        RenameSource::File(file) => Some(file),
        RenameSource::Directory(_) => None,
    }) {
        if !file_is_in_scope(file) {
            return Vec::new();
        }
        files.push(file);
    }
    files.sort_unstable_by_key(|file| file.path(db).as_ref());
    files.dedup_by(|left, right| left.path(db) == right.path(db));

    let Some(edits) = files
        .into_iter()
        .map(|file| (Db::dyn_clone(db), file))
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|(db, file)| rename_edits_for_file(&*db, file, &plan))
        .collect::<Option<Vec<_>>>()
    else {
        return Vec::new();
    };

    normalize_edits(edits.into_iter().flatten().collect()).unwrap_or_default()
}

/// A Python module or package-directory rename received from the client.
#[derive(Debug, Clone)]
pub struct PathRename {
    old_path: SystemPathBuf,
    new_path: SystemPathBuf,
    kind: PathRenameKind,
}

impl PathRename {
    /// Creates a file rename from `old_path` to `new_path`.
    pub fn file(old_path: SystemPathBuf, new_path: SystemPathBuf) -> Self {
        Self {
            old_path,
            new_path,
            kind: PathRenameKind::File,
        }
    }

    /// Creates a package-directory rename from `old_path` to `new_path`.
    pub fn directory(old_path: SystemPathBuf, new_path: SystemPathBuf) -> Self {
        Self {
            old_path,
            new_path,
            kind: PathRenameKind::Directory,
        }
    }

    fn conflicts_with(&self, other: &Self) -> bool {
        paths_overlap(&self.old_path, &other.old_path)
            || paths_overlap(&self.old_path, &other.new_path)
            || paths_overlap(&self.new_path, &other.old_path)
            || paths_overlap(&self.new_path, &other.new_path)
    }
}

/// A text replacement to apply before renaming a Python module or package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRenameEdit {
    file: File,
    range: TextRange,
    new_text: String,
}

impl FileRenameEdit {
    /// Returns the file, source range, and replacement text that make up this edit.
    pub fn into_parts(self) -> (File, TextRange, String) {
        (self.file, self.range, self.new_text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathRenameKind {
    File,
    Directory,
}

struct RenamePlan {
    rules: Vec<ResolvedRename>,
}

impl RenamePlan {
    fn new(db: &dyn Db, renames: &[PathRename]) -> Option<Self> {
        for (index, rename) in renames.iter().enumerate() {
            if renames[index + 1..]
                .iter()
                .any(|other| rename.conflicts_with(other))
            {
                return None;
            }
        }

        let mut rules: Vec<_> = renames
            .iter()
            .map(|rename| ResolvedRename::from_path_rename(db, rename))
            .collect::<Option<_>>()?;
        if rules
            .iter()
            .enumerate()
            .any(|(index, rule)| rules[index + 1..].iter().any(|other| rule.overlaps(other)))
        {
            return None;
        }
        rules.retain(|rule| rule.old_name != rule.new_name);
        (!rules.is_empty()).then_some(Self { rules })
    }

    fn rewrite_module<'a>(
        &'a self,
        db: &dyn Db,
        module: Module<'_>,
    ) -> Option<(&'a ResolvedRename, ModuleName)> {
        self.rules
            .iter()
            .find_map(|rule| rule.rewrite_module(db, module).map(|name| (rule, name)))
    }

    fn source_mentions_rule(&self, source: &str) -> bool {
        self.rules.iter().any(|rule| {
            contains_identifier(source, rule.old_name.first_component())
                || contains_identifier(source, rule.old_name.last_component())
        })
    }
}

struct ResolvedRename {
    old_name: ModuleName,
    new_name: ModuleName,
    source: RenameSource,
    definition_files: Vec<File>,
}

impl ResolvedRename {
    fn from_path_rename(db: &dyn Db, rename: &PathRename) -> Option<Self> {
        let old_path = SystemPath::absolute(&rename.old_path, db.system().current_directory());
        let new_path = SystemPath::absolute(&rename.new_path, db.system().current_directory());

        match rename.kind {
            PathRenameKind::File => {
                let extension = old_path.extension()?;
                if !matches!(extension, "py" | "pyi")
                    || new_path.extension() != Some(extension)
                    || old_path.file_stem() == Some("__init__")
                    || new_path.file_stem() == Some("__init__")
                {
                    return None;
                }
                let file = system_path_to_file(db, &old_path).ok()?;
                let old_name = file_to_module(db, file)?.name(db).clone();
                let new_name =
                    module_name_for_path(db, &new_path, PathRenameKind::File, file.is_stub(db))?;
                if resolved_source_file(db, &old_name)? != file {
                    return None;
                }
                Some(Self {
                    definition_files: resolved_module_files(db, &old_name),
                    old_name,
                    new_name,
                    source: RenameSource::File(file),
                })
            }
            PathRenameKind::Directory => {
                if !db.system().is_directory(&old_path) || new_path.starts_with(&old_path) {
                    return None;
                }
                let init_path = [old_path.join("__init__.py"), old_path.join("__init__.pyi")]
                    .into_iter()
                    .find(|path| db.system().is_file(path))?;
                let init_file = system_path_to_file(db, &init_path).ok()?;
                let old_name = file_to_module(db, init_file)?.name(db).clone();
                let new_name = module_name_for_path(
                    db,
                    &new_path,
                    PathRenameKind::Directory,
                    init_file.is_stub(db),
                )?;
                if old_name.parent() != new_name.parent() {
                    return None;
                }
                let definition_files = resolved_module_files(db, &old_name);
                if definition_files.iter().any(|file| {
                    is_legacy_namespace_package(db, *file) || !file_is_within(db, *file, &old_path)
                }) || resolved_source_file(db, &old_name)
                    .is_none_or(|file| !file_is_within(db, file, &old_path))
                {
                    return None;
                }
                Some(Self {
                    old_name,
                    new_name,
                    source: RenameSource::Directory(old_path),
                    definition_files,
                })
            }
        }
    }

    fn rewrite_module(&self, db: &dyn Db, module: Module<'_>) -> Option<ModuleName> {
        if !self.applies_to_module(db, module) {
            return None;
        }
        let name = module.name(db);
        if name == &self.old_name {
            return Some(self.new_name.clone());
        }
        let RenameSource::Directory(_) = self.source else {
            return None;
        };
        let suffix = name.relative_to(&self.old_name)?;
        let mut new_name = self.new_name.clone();
        new_name.extend(&suffix);
        Some(new_name)
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.rewrites_name(&other.old_name) || other.rewrites_name(&self.old_name)
    }

    fn rewrites_name(&self, name: &ModuleName) -> bool {
        name == &self.old_name
            || matches!(self.source, RenameSource::Directory(_))
                && name.relative_to(&self.old_name).is_some()
    }

    fn applies_to_module(&self, db: &dyn Db, module: Module<'_>) -> bool {
        let Some(file) = resolved_source_file(db, module.name(db)).or_else(|| module.file(db))
        else {
            return false;
        };
        match &self.source {
            RenameSource::File(source) => file == *source,
            RenameSource::Directory(root) => file_is_within(db, file, root),
        }
    }

    fn moves_file(&self, db: &dyn Db, file: File) -> bool {
        match &self.source {
            RenameSource::File(source) => *source == file,
            RenameSource::Directory(root) => file_is_within(db, file, root),
        }
    }

    fn unaliased_import_is_supported(&self, old_name: &ModuleName, new_name: &ModuleName) -> bool {
        matches!(self.source, RenameSource::Directory(_)) || old_name.parent() == new_name.parent()
    }
}

enum RenameSource {
    File(File),
    Directory(SystemPathBuf),
}

fn file_is_within(db: &dyn Db, file: File, root: &SystemPath) -> bool {
    file.path(db)
        .as_system_path()
        .is_some_and(|path| path.starts_with(root))
}

fn resolved_module_files(db: &dyn Db, name: &ModuleName) -> Vec<File> {
    let mut files: Vec<_> = [
        resolve_module_confident(db, name).and_then(|module| module.file(db)),
        resolve_real_module_confident(db, name).and_then(|module| module.file(db)),
    ]
    .into_iter()
    .flatten()
    .collect();
    files.dedup();
    files
}

fn resolved_source_file(db: &dyn Db, name: &ModuleName) -> Option<File> {
    resolve_real_module_confident(db, name)
        .or_else(|| resolve_module_confident(db, name))
        .and_then(|module| module.file(db))
}

fn module_name_for_path(
    db: &dyn Db,
    path: &SystemPath,
    kind: PathRenameKind,
    source_is_stub: bool,
) -> Option<ModuleName> {
    let path = SystemPath::absolute(path, db.system().current_directory());
    for search_path in search_paths(db, ModuleResolveMode::StubsAllowed) {
        let Some(root) = search_path.as_system_path() else {
            continue;
        };
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        if search_path.is_standard_library() {
            return None;
        }
        let (directory, file_name) = match kind {
            PathRenameKind::File => (relative.parent()?, Some(path.file_stem()?)),
            PathRenameKind::Directory => (relative, None),
        };
        let mut components: Vec<_> = directory
            .components()
            .map(|component| component.as_str())
            .collect();
        if source_is_stub && let Some(first) = components.first_mut() {
            *first = first.strip_suffix("-stubs").unwrap_or(first);
        }
        components.extend(file_name);
        if let Some(name) = ModuleName::from_components(components) {
            return Some(name);
        }
    }
    None
}

fn rename_edits_for_file(
    db: &dyn Db,
    file: File,
    plan: &RenamePlan,
) -> Option<Vec<FileRenameEdit>> {
    let source = source_text(db, file);
    if source.read_error().is_some() {
        return None;
    }
    let moved_by = plan.rules.iter().find(|rule| rule.moves_file(db, file));
    if !moved_by.is_some_and(|rename| matches!(rename.source, RenameSource::File(_)))
        && !plan.source_mentions_rule(source.as_str())
    {
        return Some(Vec::new());
    }

    let parsed = ruff_db::parsed::parsed_module(db, file);
    let module = parsed.load(db);
    let model = SemanticModel::new(db, file);
    let mut visitor = ImportRenameVisitor {
        db,
        model: &model,
        plan,
        moved_by,
        edits: Vec::new(),
        supported: true,
    };
    AnyNodeRef::from(module.syntax()).visit_source_order(&mut visitor);
    let mut edits = visitor.supported.then_some(visitor.edits)?;

    for rename in &plan.rules {
        let references = module_references_for_file(
            db,
            file,
            rename.definition_files.iter().copied(),
            rename.old_name.last_component(),
        )?;
        if rename.old_name.last_component() == rename.new_name.last_component() {
            continue;
        }
        for reference in references {
            edits.push((
                reference.range(),
                rename.new_name.last_component().to_string(),
            ));
        }
    }

    Some(
        edits
            .into_iter()
            .map(|(range, new_text)| FileRenameEdit {
                file,
                range,
                new_text,
            })
            .collect(),
    )
}

struct ImportRenameVisitor<'a, 'db> {
    db: &'db dyn Db,
    model: &'a SemanticModel<'db>,
    plan: &'a RenamePlan,
    moved_by: Option<&'a ResolvedRename>,
    edits: Vec<(TextRange, String)>,
    supported: bool,
}

impl<'a> SourceOrderVisitor<'a> for ImportRenameVisitor<'_, '_> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        if !self.supported {
            return TraversalSignal::Skip;
        }
        match node {
            AnyNodeRef::StmtImport(import) => {
                self.handle_import(import);
                TraversalSignal::Skip
            }
            AnyNodeRef::StmtImportFrom(import) => {
                self.handle_import_from(import);
                TraversalSignal::Skip
            }
            AnyNodeRef::ExprAttribute(attribute) => {
                self.reject_direct_qualifier(attribute);
                TraversalSignal::Traverse
            }
            AnyNodeRef::ExprName(name) => {
                self.handle_name(name);
                TraversalSignal::Traverse
            }
            AnyNodeRef::ExprStringLiteral(string) => {
                self.handle_string_annotation(string);
                TraversalSignal::Skip
            }
            _ => TraversalSignal::Traverse,
        }
    }
}

impl ImportRenameVisitor<'_, '_> {
    fn reject_direct_qualifier(&mut self, attribute: &ast::ExprAttribute) {
        let Some(module) = module_from_type(self.model, attribute) else {
            return;
        };
        let old_name = module.name(self.db);
        let Some((rename, new_name)) = self.plan.rewrite_module(self.db, module) else {
            return;
        };
        let old_parent = old_name.parent();
        if module_from_type(self.model, attribute.value.as_ref())
            .is_none_or(|module| old_parent.as_ref() != Some(module.name(self.db)))
        {
            return;
        }
        if matches!(&rename.source, RenameSource::File(_)) && old_parent != new_name.parent() {
            self.supported = false;
            return;
        }
        if attribute.attr.as_str() == old_name.last_component()
            && attribute.attr.as_str() != new_name.last_component()
        {
            self.edits
                .push((attribute.attr.range, new_name.last_component().to_string()));
        }
    }

    fn handle_name(&mut self, name: &ast::ExprName) {
        let name_text = name.id.as_str();
        let Some(rename) = self.plan.rules.iter().find(|rename| {
            matches!(rename.source, RenameSource::Directory(_))
                && rename.old_name.parent().is_none()
                && rename.old_name.last_component() == name_text
        }) else {
            return;
        };
        let definitions = definitions_for_name(self.model, name_text, name.into(), PreserveAliases);
        let moved_module = |definition: &ResolvedDefinition<'_>| matches!(definition, ResolvedDefinition::Module(file) if rename.moves_file(self.db, *file));
        if !definitions.is_empty() && definitions.iter().all(&moved_module) {
            self.edits
                .push((name.range, rename.new_name.last_component().to_string()));
        } else if definitions.iter().any(moved_module) {
            self.supported = false;
        }
    }

    fn handle_string_annotation(&mut self, string: &ast::ExprStringLiteral) {
        let Some((sub_ast, sub_model)) = self.model.enter_string_annotation(string) else {
            return;
        };
        let mut visitor = ImportRenameVisitor {
            db: self.db,
            model: &sub_model,
            plan: self.plan,
            moved_by: None,
            edits: Vec::new(),
            supported: true,
        };
        visitor.visit_expr(sub_ast.expr());
        self.supported &= visitor.supported;
        self.edits.extend(visitor.edits);
    }

    fn handle_import(&mut self, import: &ast::StmtImport) {
        for alias in &import.names {
            let Some(module) = self.model.resolve_module(Some(alias.name.as_str()), 0) else {
                continue;
            };
            let Some((rename, new_name)) = self.plan.rewrite_module(self.db, module) else {
                continue;
            };
            let old_name = module.name(self.db);
            if alias.asname.is_none() && !rename.unaliased_import_is_supported(old_name, &new_name)
            {
                self.supported = false;
                return;
            }
            if let Some(edit) = repeated_alias_edit(alias, old_name, &new_name) {
                self.edits.push(edit);
            }
            if alias.name.as_str() != new_name.as_str() {
                self.edits
                    .push((alias.name.range, new_name.as_str().to_string()));
            }
        }
    }

    fn handle_import_from(&mut self, import: &ast::StmtImportFrom) {
        let relative_move = self.moved_by.filter(|rename| {
            import.level > 0
                && matches!(rename.source, RenameSource::File(_))
                && rename.old_name.parent() != rename.new_name.parent()
        });
        let Ok(old_parent) = ModuleName::from_import_statement(self.db, self.model.file(), import)
        else {
            self.supported = relative_move.is_none();
            return;
        };
        let parent_module = self.model.resolve_module(
            import.module.as_ref().map(ast::Identifier::as_str),
            import.level,
        );
        let default_parent = parent_module
            .and_then(|module| self.plan.rewrite_module(self.db, module))
            .map(|(_, name)| name)
            .unwrap_or_else(|| old_parent.clone());

        let mut statement_parent = None;
        let mut alias_edits = Vec::new();
        for alias in &import.names {
            let alias_module = module_from_type(self.model, alias)
                .filter(|module| alias.name.as_str() == module.name(self.db).last_component());
            let alias_parent = if let Some(module) = alias_module {
                let old_name = module.name(self.db);
                if let Some((_, new_name)) = self.plan.rewrite_module(self.db, module)
                    && old_name.parent().as_ref() == Some(&old_parent)
                {
                    if let Some(edit) = repeated_alias_edit(alias, old_name, &new_name) {
                        alias_edits.push(edit);
                    }
                    if alias.name.as_str() != new_name.last_component() {
                        alias_edits.push((alias.name.range, new_name.last_component().to_string()));
                    }
                    let Some(parent) = new_name.parent() else {
                        self.supported = false;
                        return;
                    };
                    parent
                } else {
                    default_parent.clone()
                }
            } else {
                default_parent.clone()
            };

            if statement_parent
                .as_ref()
                .is_some_and(|parent| parent != &alias_parent)
            {
                self.supported = false;
                return;
            }
            statement_parent.get_or_insert(alias_parent);
        }

        let statement_parent = statement_parent.unwrap_or_else(|| old_parent.clone());
        if let Some(rename) = relative_move {
            let old_base = rename.old_name.ancestors().nth(import.level as usize);
            let new_base = rename.new_name.ancestors().nth(import.level as usize);
            let Some((old_base, mut new_base)) = old_base.zip(new_base) else {
                self.supported = false;
                return;
            };
            if old_base != new_base {
                if let Some(suffix) = old_parent.relative_to(&old_base) {
                    new_base.extend(&suffix);
                }
                if new_base != statement_parent {
                    self.supported = false;
                    return;
                }
                if new_base != old_parent {
                    self.edits.extend(alias_edits);
                    return;
                }
            }
        }
        let moved_directory = self
            .moved_by
            .is_some_and(|rename| matches!(rename.source, RenameSource::Directory(_)));
        if statement_parent != old_parent {
            let Some(module) = &import.module else {
                if moved_directory {
                    self.edits.extend(alias_edits);
                    return;
                }
                self.supported = false;
                return;
            };
            let replacement = if import.level == 0 {
                statement_parent.as_str().to_string()
            } else {
                let Some(replacement) =
                    relative_module_replacement(module.as_str(), &old_parent, &statement_parent)
                else {
                    if moved_directory {
                        self.edits.extend(alias_edits);
                        return;
                    }
                    self.supported = false;
                    return;
                };
                replacement
            };
            if replacement != module.as_str() {
                self.edits.push((module.range, replacement));
            }
        }
        self.edits.extend(alias_edits);
    }
}

fn module_from_type<'db, T: HasType>(
    model: &SemanticModel<'db>,
    expression: &T,
) -> Option<Module<'db>> {
    let Type::ModuleLiteral(literal) = expression.inferred_type(model)? else {
        return None;
    };
    Some(literal.module(model.db()))
}

fn repeated_alias_edit(
    alias: &ast::Alias,
    old_name: &ModuleName,
    new_name: &ModuleName,
) -> Option<(TextRange, String)> {
    let asname = alias.asname.as_ref()?;
    (asname.as_str() == old_name.last_component() && asname.as_str() != new_name.last_component())
        .then(|| (asname.range, new_name.last_component().to_string()))
}

fn relative_module_replacement(
    text: &str,
    old_name: &ModuleName,
    new_name: &ModuleName,
) -> Option<String> {
    let suffix_len = text.split('.').count();
    let old_components: Vec<_> = old_name.components().collect();
    let new_components: Vec<_> = new_name.components().collect();
    if old_components.len() != new_components.len() || suffix_len > old_components.len() {
        return None;
    }
    let prefix_len = old_components.len() - suffix_len;
    (old_components[..prefix_len] == new_components[..prefix_len])
        .then(|| new_components[prefix_len..].join("."))
}

fn normalize_edits(mut edits: Vec<FileRenameEdit>) -> Option<Vec<FileRenameEdit>> {
    edits.sort_by(|left, right| {
        left.file
            .cmp(&right.file)
            .then_with(|| left.range.start().cmp(&right.range.start()))
            .then_with(|| left.range.end().cmp(&right.range.end()))
            .then_with(|| left.new_text.cmp(&right.new_text))
    });
    edits.dedup();
    (!edits.windows(2).any(|edits| {
        edits[0].file == edits[1].file && ranges_overlap(edits[0].range, edits[1].range)
    }))
    .then_some(edits)
}

fn ranges_overlap(left: TextRange, right: TextRange) -> bool {
    left.start() < right.end() && right.start() < left.end()
}

fn paths_overlap(left: &SystemPath, right: &SystemPath) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_db::files::system_path_to_file;
    use ruff_db::source::source_text;
    use ruff_db::system::DbWithWritableSystem;
    use ruff_python_ast::PythonVersion;
    use ty_project::{ProjectMetadata, TestDb};

    #[test]
    fn file_move_rewrites_imports_and_semantic_references() {
        let db = create_test_db(&[
            ("/pkg/__init__.py", ""),
            ("/pkg/old.py", "class C:\n    pass\n"),
            (
                "/pkg/consumer.py",
                "import pkg.old\nfrom . import old as imported\nvalue: 'pkg.old.C'\nresult = pkg.old.C()\ndef shadowed(old):\n    return old.C\nprint(imported.C)\n",
            ),
        ]);

        assert_file_move(
            &db,
            "/pkg/old.py",
            "/pkg/new.py",
            "/pkg/consumer.py",
            "import pkg.new\nfrom . import new as imported\nvalue: 'pkg.new.C'\nresult = pkg.new.C()\ndef shadowed(old):\n    return old.C\nprint(imported.C)\n",
        );
    }

    #[test]
    fn cross_parent_file_move_supports_stable_import_shapes() {
        let source = "from .. import c\nfrom ..o import X\nfrom . import t\n";
        let db = create_test_db(&[
            ("/p/__init__.py", ""),
            ("/p/c.py", ""),
            ("/p/o.py", "X=1\n"),
            ("/p/a/__init__.py", ""),
            ("/p/a/one.py", source),
            ("/p/a/t.py", ""),
            ("/p/b/__init__.py", ""),
            (
                "/consumer.py",
                "import p.a.one as stable\nfrom p.a.one import X\nfrom p.a import one\nprint(stable.X, X, one.X)\n",
            ),
        ]);
        let edits = will_rename(
            &db,
            &[
                PathRename::file("/p/a/one.py".into(), "/p/b/new.py".into()),
                PathRename::file("/p/a/t.py".into(), "/p/b/t.py".into()),
                PathRename::file("/p/o.py".into(), "/p/n.py".into()),
            ],
        );

        assert_eq!(
            apply_edits(&db, &edits, "/consumer.py"),
            "import p.b.new as stable\nfrom p.b.new import X\nfrom p.b import new\nprint(stable.X, X, new.X)\n"
        );
        assert_eq!(
            apply_edits(&db, &edits, "/p/a/one.py"),
            "from .. import c\nfrom ..n import X\nfrom . import t\n"
        );
    }

    #[test]
    fn one_unrepresentable_occurrence_cancels_the_file_item() {
        for (new_path, unsupported) in [
            (
                "/new_pkg/old.py",
                "import old_pkg\nimport old_pkg.old as stable\nprint(old_pkg.old.x, stable.x)\n",
            ),
            (
                "/new_pkg/new.py",
                "if x:\n from old_pkg import old\nelse:\n import other as old\nold.x\n",
            ),
        ] {
            let db = create_test_db(&[
                ("/old_pkg/__init__.py", "other = 1\n"),
                ("/old_pkg/old.py", "x = 1\n"),
                ("/new_pkg/__init__.py", ""),
                ("/other.py", "x = 2\n"),
                (
                    "/safe.py",
                    "import old_pkg.old as stable\nprint(stable.x)\n",
                ),
                ("/unsupported.py", unsupported),
            ]);
            let edits = will_rename(
                &db,
                &[PathRename::file("/old_pkg/old.py".into(), new_path.into())],
            );

            assert!(edits.is_empty());
        }
    }

    #[test]
    fn regular_package_folder_rename_rewrites_the_prefix() {
        let db = create_test_db(&[
            ("/pkg/__init__.py", ""),
            (
                "/pkg/old/__init__.py",
                "from . import api\nfrom .api import Client\nfrom ..old import api as package_api\n",
            ),
            ("/pkg/old/__init__.pyi", ""),
            ("/pkg/old/api.py", "class Client:\n    pass\n"),
            (
                "/consumer.py",
                "import pkg.old.api\nfrom pkg import old\nvalue: 'pkg.old.api.Client'\nprint(pkg.old.api.Client, old.api.Client)\n",
            ),
        ]);
        let edits = will_rename(
            &db,
            &[PathRename::directory("/pkg/old".into(), "/pkg/new".into())],
        );

        assert_eq!(
            apply_edits(&db, &edits, "/consumer.py"),
            "import pkg.new.api\nfrom pkg import new\nvalue: 'pkg.new.api.Client'\nprint(pkg.new.api.Client, new.api.Client)\n"
        );
        assert_eq!(
            apply_edits(&db, &edits, "/pkg/old/__init__.py"),
            "from . import api\nfrom .api import Client\nfrom ..new import api as package_api\n"
        );
    }

    #[test]
    fn unsupported_rename_shapes_produce_no_edits() {
        for source in ["from . import helper\n", "from ....x import y\n"] {
            assert_no_edits(
                &[
                    ("/old_pkg/__init__.py", ""),
                    ("/old_pkg/helper.py", ""),
                    ("/old_pkg/old.py", source),
                    ("/new_pkg/__init__.py", ""),
                    ("/c.py", "import old_pkg.old as x\n"),
                ],
                &[PathRename::file(
                    "/old_pkg/old.py".into(),
                    "/new_pkg/new.py".into(),
                )],
            );
        }
        assert_no_edits(
            &[
                ("/a/old/__init__.py", ""),
                ("/b/__init__.py", ""),
                ("/consumer.py", "import a.old as stable\nprint(stable)\n"),
            ],
            &[PathRename::directory("/a/old".into(), "/b/new".into())],
        );
        assert_no_edits(
            &[("/pkg/__init__.py", ""), ("/pkg/old.py", "")],
            &[
                PathRename::directory("/pkg".into(), "/new_pkg".into()),
                PathRename::file("/pkg/old.py".into(), "/elsewhere.py".into()),
            ],
        );
        assert_no_edits(
            &[
                ("/a/old.py", ""),
                ("/c/old.py", ""),
                (
                    "/use.py",
                    "if x:\n import a.old as old\nelse:\n import c.old as old\nimport a.old as stable\nold.x\n",
                ),
            ],
            &[PathRename::file("/a/old.py".into(), "/b/old.py".into())],
        );
        let legacy = "__import__('pkg_resources').declare_namespace(__name__)\n";
        assert_no_edits(
            &[
                ("/o/__init__.py", ""),
                ("/o/__init__.pyi", legacy),
                ("/u.py", "import o\n"),
            ],
            &[PathRename::directory("/o".into(), "/n".into())],
        );
    }

    #[test]
    fn import_rewrites_follow_the_runtime_source() {
        let db = create_test_db(&[
            ("/old.py", "x = 1\n"),
            ("/old.pyi", "x: int\n"),
            ("/consumer.py", "import old\nprint(old.x)\n"),
        ]);
        assert_file_move(
            &db,
            "/old.pyi",
            "/new.pyi",
            "/consumer.py",
            "import old\nprint(old.x)\n",
        );
        assert_file_move(
            &db,
            "/old.py",
            "/new.py",
            "/consumer.py",
            "import new\nprint(new.x)\n",
        );
        let stubs = create_test_db(&[
            ("/old-stubs/__init__.pyi", ""),
            ("/old-stubs/sub.pyi", "class C: ...\n"),
            ("/use.py", "import old.sub\nvalue: old.sub.C\n"),
            (
                "/a.py",
                "import old.sub as direct\nimport l\nfrom l import old\nprint(direct.C, l.old.C, old.C)\n",
            ),
            ("/l.py", "import old as x\nold = x\nold.C\n"),
        ]);
        let edits = will_rename(
            &stubs,
            &[PathRename::directory(
                "/old-stubs".into(),
                "/new-stubs".into(),
            )],
        );
        assert_eq!(
            apply_edits(&stubs, &edits, "/use.py"),
            "import new.sub\nvalue: new.sub.C\n"
        );
        assert_eq!(
            apply_edits(&stubs, &edits, "/l.py"),
            "import new as x\nold = x\nold.C\n"
        );
        assert_eq!(
            apply_edits(&stubs, &edits, "/a.py"),
            "import new.sub as direct\nimport l\nfrom l import old\nprint(direct.C, l.old.C, old.C)\n"
        );

        assert_no_edits(
            &[
                ("/old.pyi", "class C: ...\n"),
                ("/old/sub.py", ""),
                ("/use.py", "import old\nvalue: old.C\n"),
            ],
            &[PathRename::file("/old.pyi".into(), "/new.pyi".into())],
        );
    }

    fn will_rename(db: &dyn Db, renames: &[PathRename]) -> Vec<FileRenameEdit> {
        let files = db.project().files(db);
        will_rename_paths(db, renames, &files, |_| true)
    }

    fn assert_file_move(db: &dyn Db, old_path: &str, new_path: &str, target: &str, expected: &str) {
        let edits = will_rename(db, &[PathRename::file(old_path.into(), new_path.into())]);
        assert_eq!(apply_edits(db, &edits, target), expected);
    }

    fn assert_no_edits(files: &[(&str, &str)], renames: &[PathRename]) {
        let db = create_test_db(files);
        assert!(will_rename(&db, renames).is_empty());
    }

    fn create_test_db(files: &[(&str, &str)]) -> TestDb {
        let mut db = TestDb::new(ProjectMetadata::new("test".into(), "/".into()));
        db.init_program_with_python_version(PythonVersion::latest_ty())
            .unwrap();
        for &(path, contents) in files {
            db.write_file(path, contents)
                .expect("write to memory file system to be successful");
        }
        db
    }

    fn apply_edits(db: &dyn Db, edits: &[FileRenameEdit], path: &str) -> String {
        let file = system_path_to_file(db, path).unwrap();
        let mut sorted_edits: Vec<_> = edits.iter().filter(|edit| edit.file == file).collect();
        sorted_edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start()));

        let mut result = source_text(db, file).as_str().to_owned();
        for edit in sorted_edits {
            let start = usize::from(edit.range.start());
            let end = usize::from(edit.range.end());
            result.replace_range(start..end, &edit.new_text);
        }
        result
    }
}
