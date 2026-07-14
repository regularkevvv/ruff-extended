use compact_str::CompactString;
use itertools::{Either, Itertools};
use ruff_db::{
    diagnostic::{Annotation, Diagnostic, DiagnosticId, Severity, Span},
    files::File,
    parsed::{ParsedModuleRef, parsed_module},
    source::{line_index, source_text},
};
use ruff_python_ast as ast;
use ruff_python_ast::{PythonVersion, name::Name};
use ruff_source_file::{OneIndexed, PositionEncoding, SourceLocation};
use ruff_text_size::{Ranged, TextRange};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use ty_module_resolver::all_modules;
use ty_plugin_protocol as protocol;

use crate::types::plugin::{
    PluginVirtualTypePatch, plugin_callable_type_from_protocol_signature_in_class,
    plugin_callable_type_from_protocol_signature_with_virtual_types, plugin_semantic_context,
    plugin_type_expr_from_type, plugin_type_expr_to_type_in_class_with_virtual_types,
    plugin_type_expr_to_type_with_virtual_types, plugin_virtual_type_patches_from_protocol,
};
use crate::{
    Db, FxIndexMap, FxIndexSet, Program, TypeQualifiers,
    place::{
        DefinedPlace, Definedness, Place, PlaceAndQualifiers, Provenance, PublicTypePolicy,
        TypeOrigin, place_from_bindings, place_from_declarations,
    },
    reachability::{DeclarationsIteratorExtension, binding_reachability},
    types::{
        ApplyTypeMappingVisitor, BoundTypeVarIdentity, BoundTypeVarInstance, CallArguments,
        CallableType, ClassBase, ClassLiteral, ClassType, DATACLASS_FLAGS, DataclassFlags,
        DataclassParams, GenericAlias, GenericContext, KnownClass, KnownInstanceType,
        MaterializationKind, MemberLookupPolicy, MetaclassCandidate, MetaclassTransformInfo,
        Parameter, Parameters, PropertyInstanceType, Signature, SpecialFormType, StaticMroError,
        SubclassOfType, Truthiness, Type, TypeContext, TypeMapping, TypeVarVariance,
        TypedDictModule, UnionBuilder, UnionType,
        call::{CallError, CallErrorKind},
        callable::{CallableFunctionProvenance, CallableTypeKind},
        class::{
            ClassInstanceFlags, ClassMemberResult, CodeGeneratorKind, DisjointBase,
            DynamicTypedDictLiteral, Field, FieldKind, InstanceMemberResult, MetaclassError,
            MetaclassErrorKind, MethodDecorator, MroLookup, NamedTupleField, SlotsKind,
            synthesize_namedtuple_class_member,
            typed_dict::{TypedDictFields, synthesize_typed_dict_method, typed_dict_class_member},
        },
        context::InferContext,
        dedicated::pydantic,
        definition_expression_type, determine_upper_bound,
        diagnostic::INVALID_DATACLASS_OVERRIDE,
        enums::{enum_metadata, is_enum_class_by_inheritance, try_unwrap_nonmember_value},
        function::{
            DataclassTransformerParams, KnownFunction, is_implicit_classmethod,
            is_implicit_staticmethod,
        },
        generics::Specialization,
        infer::infer_unpack_types,
        infer_expression_type, inferred_declaration,
        known_instance::DeprecatedInstance,
        member::{Member, class_member},
        mro::{Mro, MroIterator},
        signatures::{CallableSignature, ParametersKind},
        tuple::{FixedLengthTuple, Tuple},
        typed_dict::{TypedDictParams, TypedDictType, typed_dict_params_from_class_def},
        variance::VarianceInferable,
        visitor::{TypeCollector, TypeVisitor, walk_type_with_recursion_guard},
    },
};
use crate::{attribute_assignments, attribute_declarations};
use ty_python_core::{
    attribute_scopes,
    definition::{Definition, DefinitionKind, DefinitionState, TargetKind},
    place_table,
    program::{SemanticPlugin, SemanticPluginRuntime},
    scope::{Scope, ScopeId},
    semantic_index,
    symbol::Symbol,
    use_def_map,
};

/// Representation of a class definition statement in the AST: either a non-generic class, or a
/// generic class that has not been specialized.
///
/// This does not in itself represent a type, but can be transformed into a [`ClassType`] that
/// does. (For generic classes, this requires specializing its generic context.)
#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct StaticClassLiteral<'db> {
    /// Name of the class at definition
    #[returns(ref)]
    pub(crate) name: Name,

    #[returns(copy)]
    pub(crate) body_scope: ScopeId<'db>,

    #[returns(copy)]
    pub(crate) known: Option<KnownClass>,

    /// If this class is deprecated, this holds the deprecation message.
    #[returns(copy)]
    pub(crate) deprecated: Option<DeprecatedInstance<'db>>,

    #[returns(copy)]
    pub(crate) type_check_only: bool,

    #[returns(copy)]
    pub(crate) dataclass_params: Option<DataclassParams<'db>>,
    #[returns(copy)]
    pub(crate) dataclass_transformer_params: Option<DataclassTransformerParams<'db>>,

    /// Whether this class is decorated with `@functools.total_ordering`
    #[returns(copy)]
    pub(crate) total_ordering: bool,

    /// Whether this class has any decorators.
    #[returns(copy)]
    pub(crate) has_decorators: bool,

    /// Whether this class has PEP 695 type parameters.
    #[returns(copy)]
    pub(crate) has_type_params: bool,

    /// Whether this class has any explicit base classes.
    #[returns(copy)]
    pub(crate) has_explicit_bases: bool,

    /// Whether this class has an explicit `metaclass` keyword argument.
    #[returns(copy)]
    pub(crate) has_explicit_metaclass: bool,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for StaticClassLiteral<'_> {}

#[derive(Clone, Debug, Default, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
struct PluginClassTransformPatch<'db> {
    fields: Box<[PluginClassFieldPatch<'db>]>,
    class_members: Box<[PluginMemberPatch<'db>]>,
    instance_members: Box<[PluginMemberPatch<'db>]>,
    constructor: Option<PluginConstructorPatch<'db>>,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
struct PluginClassFieldPatch<'db> {
    name: Name,
    replace_existing: bool,
    descriptor_class_ty: Option<Type<'db>>,
    instance_get_ty: Type<'db>,
    instance_set_ty: Option<Type<'db>>,
    has_default: bool,
    constructor_parameter: Option<PluginConstructorParameter<'db>>,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
struct PluginMemberPatch<'db> {
    name: Name,
    replace_existing: bool,
    ty: Type<'db>,
    read_only: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
struct PluginProjectIndex<'db> {
    plugin_index_json: Option<String>,
    contributions: Box<[PluginContributionPatch<'db>]>,
    virtual_types: Box<[PluginVirtualTypePatch<'db>]>,
    diagnostics: Box<[PluginProjectDiagnostic]>,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
struct PluginContributionPatch<'db> {
    target: PluginContributionTarget,
    patch: PluginContributionMemberPatch<'db>,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
enum PluginContributionMemberPatch<'db> {
    Member(PluginMemberPatch<'db>),
    Field(PluginContributionFieldPatch<'db>),
    Constructor(PluginConstructorPatch<'db>),
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
struct PluginContributionFieldPatch<'db> {
    name: Name,
    replace_existing: bool,
    descriptor_class_ty: Option<Type<'db>>,
    instance_get_ty: Type<'db>,
    instance_set_ty: Option<Type<'db>>,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize)]
enum PluginContributionTarget {
    Class(String),
    Instance(String),
    Constructor(String),
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize)]
struct PluginProjectDiagnostic {
    id: String,
    message: String,
    severity: PluginProjectDiagnosticSeverity,
    location: Option<PluginProjectDiagnosticLocation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, get_size2::GetSize)]
enum PluginProjectDiagnosticSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize)]
struct PluginProjectDiagnosticLocation {
    file_path: String,
    start_line: u32,
    start_column: u32,
    end_line: u32,
    end_column: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
struct PluginConstructorPatch<'db> {
    parameters: Box<[PluginConstructorParameter<'db>]>,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
struct PluginConstructorParameter<'db> {
    name: Option<Name>,
    kind: PluginConstructorParameterKind,
    ty: Type<'db>,
    required: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, get_size2::GetSize)]
enum PluginConstructorParameterKind {
    PositionalOnly,
    PositionalOrKeyword,
    VarArgs,
    KeywordOnly,
    Kwargs,
}

struct PluginClassFieldSummary<'db> {
    name: Name,
    annotation: Option<Type<'db>>,
    assigned_value: Option<protocol::AssignedValueSummary>,
    inferred_type: Option<Type<'db>>,
    has_default: bool,
    source: protocol::SymbolSource,
}

struct PluginClassSummary<'db> {
    fields: Vec<PluginClassFieldSummary<'db>>,
    methods: Vec<protocol::MethodSummary>,
    decorators: Vec<protocol::CallOrSymbolSummary>,
    metaclass: Option<Type<'db>>,
    nested_classes: Vec<protocol::NestedClassSummary>,
    class_constants: Vec<protocol::ConstantSummary>,
    source: protocol::SymbolSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PluginMemberScope {
    Class,
    Instance,
}

#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
struct SemanticPluginId<'db> {
    #[returns(ref)]
    id: String,
}

impl PluginContributionTarget {
    fn matches(&self, scope: PluginMemberScope, owner_qualified_name: &str) -> bool {
        match (self, scope) {
            (Self::Class(qualified_name), PluginMemberScope::Class)
            | (Self::Instance(qualified_name), PluginMemberScope::Instance) => {
                qualified_name == owner_qualified_name
            }
            _ => false,
        }
    }
}

#[salsa::tracked]
impl<'db> StaticClassLiteral<'db> {
    /// Return `true` if this class represents `known_class`
    pub(crate) fn is_known(self, db: &'db dyn Db, known_class: KnownClass) -> bool {
        self.known(db) == Some(known_class)
    }

    pub(crate) fn is_tuple(self, db: &'db dyn Db) -> bool {
        self.is_known(db, KnownClass::Tuple)
    }

    /// Returns `true` if this class inherits from a functional namedtuple
    /// (`DynamicNamedTupleLiteral`) that has unknown fields.
    ///
    /// When the base namedtuple's fields were determined dynamically (e.g., from a variable),
    /// we can't synthesize precise method signatures and should fall back to `NamedTupleFallback`.
    pub(crate) fn namedtuple_base_has_unknown_fields(self, db: &'db dyn Db) -> bool {
        self.explicit_bases(db).iter().any(|base| match base {
            Type::ClassLiteral(ClassLiteral::DynamicNamedTuple(namedtuple)) => {
                !namedtuple.has_known_fields(db)
            }
            _ => false,
        })
    }

    /// Returns `true` if this class is a dataclass-like class.
    ///
    /// This covers `@dataclass`-decorated classes, as well as classes created via
    /// `dataclass_transform` (function-based, metaclass-based, and base-class-based).
    /// This specifically excludes Pydantic models, even though their metaclass also uses
    /// `dataclass_transform`.
    pub(crate) fn is_dataclass_like(self, db: &'db dyn Db) -> bool {
        CodeGeneratorKind::from_class(db, ClassLiteral::Static(self))
            .is_some_and(CodeGeneratorKind::is_dataclass_like)
    }

    /// Returns `true` if this class is decorated with `@dataclass(order=True)`.
    pub(crate) fn is_ordered_dataclass(self, db: &'db dyn Db) -> bool {
        self.find_dataclass_decorator_position(db).is_some()
            && self
                .dataclass_params(db)
                .is_some_and(|params| params.flags(db).contains(DataclassFlags::ORDER))
    }

    /// Returns a new [`StaticClassLiteral`] with the given dataclass params, preserving all other fields.
    pub(crate) fn with_dataclass_params(
        self,
        db: &'db dyn Db,
        dataclass_params: Option<DataclassParams<'db>>,
    ) -> Self {
        Self::new(
            db,
            self.name(db),
            self.body_scope(db),
            self.known(db),
            self.deprecated(db),
            self.type_check_only(db),
            dataclass_params,
            self.dataclass_transformer_params(db),
            self.total_ordering(db),
            self.has_decorators(db),
            self.has_type_params(db),
            self.has_explicit_bases(db),
            self.has_explicit_metaclass(db),
        )
    }

    /// Returns `true` if this class defines any ordering method (`__lt__`, `__le__`, `__gt__`,
    /// `__ge__`) in its own body (not inherited). Used by `@total_ordering` to determine if
    /// synthesis is valid.
    #[salsa::tracked(returns(copy))]
    pub(crate) fn has_own_ordering_method(self, db: &'db dyn Db) -> bool {
        let body_scope = self.body_scope(db);
        ["__lt__", "__le__", "__gt__", "__ge__"]
            .iter()
            .any(|method| !class_member(db, body_scope, method).is_undefined())
    }

    #[salsa::tracked(returns(copy))]
    pub(crate) fn has_own_comparison_methods(self, db: &'db dyn Db) -> bool {
        let body_scope = self.body_scope(db);
        ["__lt__", "__le__", "__gt__", "__ge__"]
            .iter()
            .all(|method| !class_member(db, body_scope, method).is_undefined())
    }

    /// Returns `true` if any class in this class's MRO (excluding `object`) defines an ordering
    /// method (`__lt__`, `__le__`, `__gt__`, `__ge__`). Used by `@total_ordering` validation.
    pub(crate) fn has_ordering_method_in_mro(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
    ) -> bool {
        self.total_ordering_root_method(db, specialization)
            .is_some()
    }

    /// Returns the type of the ordering method used by `@total_ordering`, if any.
    ///
    /// Following `functools.total_ordering` precedence, we prefer `__lt__` > `__le__` > `__gt__` >
    /// `__ge__`, regardless of whether the method is defined locally or inherited.
    ///
    /// Note: We use direct scope lookups here to avoid infinite recursion
    /// through `own_class_member` -> `own_synthesized_member`.
    pub(super) fn total_ordering_root_method(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
    ) -> Option<Type<'db>> {
        const ORDERING_METHODS: [&str; 4] = ["__lt__", "__le__", "__gt__", "__ge__"];

        for name in ORDERING_METHODS {
            for base in self.iter_mro(db, specialization) {
                let Some(base_class) = base.into_class() else {
                    continue;
                };
                match base_class.class_literal(db) {
                    ClassLiteral::Static(base_literal) => {
                        if base_literal.is_known(db, KnownClass::Object) {
                            continue;
                        }
                        let member = class_member(db, base_literal.body_scope(db), name);
                        if let Some(ty) = member.ignore_possibly_undefined() {
                            let base_specialization = base_class
                                .static_class_literal(db)
                                .and_then(|(_, spec)| spec);
                            return Some(ty.apply_optional_specialization(db, base_specialization));
                        }
                    }
                    ClassLiteral::Dynamic(dynamic) => {
                        // Dynamic classes (created with `type()`) can also define ordering methods
                        // in their namespace dict.
                        let member = dynamic.own_class_member(db, name);
                        if let Some(ty) = member.ignore_possibly_undefined() {
                            return Some(ty);
                        }
                    }
                    ClassLiteral::DynamicNamedTuple(_)
                    | ClassLiteral::DynamicTypedDict(_)
                    | ClassLiteral::DynamicEnum(_) => {}
                }
            }
        }

        None
    }

    #[salsa::tracked(
        returns(copy),
        cycle_initial=|_, _, _| None,
        heap_size=ruff_memory_usage::heap_size,
    )]
    pub(crate) fn generic_context(self, db: &'db dyn Db) -> Option<GenericContext<'db>> {
        // Several typeshed definitions examine `sys.version_info`. To break cycles, we hard-code
        // the knowledge that this class is not generic.
        if self.is_known(db, KnownClass::VersionInfo) {
            return None;
        }

        // We've already verified that the class literal does not contain both a PEP-695 generic
        // scope and a `typing.Generic` base class.
        //
        // Note that if a class has an explicit legacy generic context (by inheriting from
        // `typing.Generic`), and also an implicit one (by inheriting from other generic classes,
        // specialized by typevars), the explicit one takes precedence.
        self.pep695_generic_context(db)
            .or_else(|| self.legacy_generic_context(db))
            .or_else(|| self.inherited_legacy_generic_context(db))
    }

    pub(crate) fn has_pep_695_type_params(self, db: &'db dyn Db) -> bool {
        self.pep695_generic_context(db).is_some()
    }

    pub(crate) fn pep695_generic_context(self, db: &'db dyn Db) -> Option<GenericContext<'db>> {
        if !self.has_type_params(db) {
            return None;
        }
        self.pep695_generic_context_inner(db)
    }

    #[salsa::tracked(
        returns(copy),
        cycle_initial=|_, _, _| None,
        heap_size=ruff_memory_usage::heap_size,
    )]
    fn pep695_generic_context_inner(self, db: &'db dyn Db) -> Option<GenericContext<'db>> {
        let scope = self.body_scope(db);
        let file = scope.file(db);
        let parsed = parsed_module(db, file).load(db);
        let class_def_node = scope.node(db).expect_class().node(&parsed);
        class_def_node.type_params.as_ref().map(|type_params| {
            let index = semantic_index(db, scope.file(db));
            let definition = index.expect_single_definition(class_def_node);
            GenericContext::from_type_params(db, index, definition, type_params)
        })
    }

    pub(crate) fn legacy_generic_context(self, db: &'db dyn Db) -> Option<GenericContext<'db>> {
        self.explicit_bases(db).iter().find_map(|base| match base {
            Type::KnownInstance(
                KnownInstanceType::SubscriptedGeneric(generic_context)
                | KnownInstanceType::SubscriptedProtocol(generic_context),
            ) => Some(*generic_context),
            _ => None,
        })
    }

    pub(crate) fn inherited_legacy_generic_context(
        self,
        db: &'db dyn Db,
    ) -> Option<GenericContext<'db>> {
        #[salsa::tracked(
            returns(copy),
            cycle_initial=|_, _, _| None,
            heap_size=ruff_memory_usage::heap_size,
        )]
        fn inherited_legacy_generic_context_inner<'db>(
            db: &'db dyn Db,
            class: StaticClassLiteral<'db>,
        ) -> Option<GenericContext<'db>> {
            GenericContext::from_base_classes(
                db,
                class.definition(db),
                class
                    .explicit_bases(db)
                    .iter()
                    .copied()
                    .filter(|ty| matches!(ty, Type::GenericAlias(_))),
            )
        }

        if !self.has_explicit_bases(db) {
            return None;
        }
        inherited_legacy_generic_context_inner(db, self)
    }

    /// Returns all of the typevars that are referenced in this class's base class list.
    /// (This is used to ensure that classes do not reference typevars from enclosing
    /// generic contexts.)
    pub(crate) fn typevars_referenced_in_bases(
        self,
        db: &'db dyn Db,
    ) -> FxIndexSet<BoundTypeVarInstance<'db>> {
        #[derive(Default)]
        struct CollectTypeVars<'db> {
            typevars: RefCell<FxIndexSet<BoundTypeVarInstance<'db>>>,
            recursion_guard: TypeCollector<'db>,
        }

        impl<'db> TypeVisitor<'db> for CollectTypeVars<'db> {
            fn should_visit_lazy_type_attributes(&self) -> bool {
                false
            }

            fn visit_bound_type_var_type(
                &self,
                _db: &'db dyn Db,
                bound_typevar: BoundTypeVarInstance<'db>,
            ) {
                self.typevars.borrow_mut().insert(bound_typevar);
            }

            fn visit_generic_alias_type(&self, db: &'db dyn Db, alias: GenericAlias<'db>) {
                // The generic context contains the base class's formal type parameters, not type
                // variables referenced by this class's base expression.
                for ty in alias.specialization(db).types(db) {
                    self.visit_type(db, *ty);
                }
            }

            fn visit_type(&self, db: &'db dyn Db, ty: Type<'db>) {
                walk_type_with_recursion_guard(db, ty, self, &self.recursion_guard);
            }
        }

        let visitor = CollectTypeVars::default();
        for base in self.explicit_bases(db) {
            visitor.visit_type(db, *base);
        }
        visitor.typevars.into_inner()
    }

    /// Returns the generic context that should be inherited by any constructor methods of this class.
    pub(super) fn inherited_generic_context(self, db: &'db dyn Db) -> Option<GenericContext<'db>> {
        self.generic_context(db)
    }

    pub(crate) fn file(self, db: &dyn Db) -> File {
        self.body_scope(db).file(db)
    }

    /// Return the original [`ast::StmtClassDef`] node associated with this class
    ///
    /// ## Note
    /// Only call this function from queries in the same file or your
    /// query depends on the AST of another file (bad!).
    fn node<'ast>(self, db: &'db dyn Db, module: &'ast ParsedModuleRef) -> &'ast ast::StmtClassDef {
        self.body_scope(db).node(db).expect_class().node(module)
    }

    pub(crate) fn definition(self, db: &'db dyn Db) -> Definition<'db> {
        let body_scope = self.body_scope(db);
        let index = semantic_index(db, body_scope.file(db));
        index.expect_single_definition(body_scope.node(db).expect_class())
    }

    pub(crate) fn apply_specialization(
        self,
        db: &'db dyn Db,
        f: impl FnOnce(GenericContext<'db>) -> Specialization<'db>,
    ) -> ClassType<'db> {
        match self.generic_context(db) {
            None => ClassType::NonGeneric(self.into()),
            Some(generic_context) => {
                let specialization = f(generic_context);

                ClassType::Generic(GenericAlias::new(db, self, specialization))
            }
        }
    }

    pub(crate) fn apply_optional_specialization(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
    ) -> ClassType<'db> {
        self.apply_specialization(db, |generic_context| {
            specialization
                .unwrap_or_else(|| generic_context.default_specialization(db, self.known(db)))
        })
    }

    pub(crate) fn top_materialization(self, db: &'db dyn Db) -> ClassType<'db> {
        self.apply_specialization(db, |generic_context| {
            generic_context
                .default_specialization(db, self.known(db))
                .materialize_impl(
                    db,
                    MaterializationKind::Top,
                    &ApplyTypeMappingVisitor::default(),
                )
        })
    }

    /// Returns the default specialization of this class. For non-generic classes, the class is
    /// returned unchanged. For a non-specialized generic class, we return a generic alias that
    /// applies the default specialization to the class's typevars.
    pub(crate) fn default_specialization(self, db: &'db dyn Db) -> ClassType<'db> {
        self.apply_specialization(db, |generic_context| {
            generic_context.default_specialization(db, self.known(db))
        })
    }

    /// Returns the unknown specialization of this class. For non-generic classes, the class is
    /// returned unchanged. For a non-specialized generic class, we return a generic alias that
    /// maps each of the class's typevars to `Unknown`.
    pub(crate) fn unknown_specialization(self, db: &'db dyn Db) -> ClassType<'db> {
        self.apply_specialization(db, |generic_context| {
            generic_context.unknown_specialization(db)
        })
    }

    /// Returns a specialization of this class where each typevar is mapped to itself.
    pub(crate) fn identity_specialization(self, db: &'db dyn Db) -> ClassType<'db> {
        self.apply_specialization(db, |generic_context| {
            generic_context.identity_specialization(db)
        })
    }

    /// Return an iterator over the inferred types of this class's *explicit* bases.
    ///
    /// Note that any class (except for `object`) that has no explicit
    /// bases will implicitly inherit from `object` at runtime. Nonetheless,
    /// this method does *not* include `object` in the bases it iterates over.
    ///
    /// ## Why is this a salsa query?
    ///
    /// This is a salsa query to short-circuit the invalidation
    /// when the class's AST node changes.
    ///
    /// Were this not a salsa query, then the calling query
    /// would depend on the class's AST and rerun for every change in that file.
    pub(crate) fn explicit_bases(self, db: &'db dyn Db) -> &'db [Type<'db>] {
        #[salsa::tracked(returns(deref), cycle_initial=explicit_bases_cycle_initial, cycle_fn=explicit_bases_cycle_fn, heap_size=ruff_memory_usage::heap_size)]
        fn explicit_bases_inner<'db>(
            db: &'db dyn Db,
            class: StaticClassLiteral<'db>,
        ) -> Box<[Type<'db>]> {
            tracing::trace!(
                "StaticClassLiteral::explicit_bases_query: {}",
                class.name(db)
            );

            let module = parsed_module(db, class.file(db)).load(db);
            let class_stmt = class.node(db, &module);

            let class_definition =
                semantic_index(db, class.file(db)).expect_single_definition(class_stmt);

            expanded_class_base_entries(db, class.known(db), class_stmt, class_definition)
                .into_iter()
                .map(ExpandedClassBaseEntry::ty)
                .collect()
        }

        if !self.has_explicit_bases(db) {
            return &[];
        }
        explicit_bases_inner(db, self)
    }

    /// Return `Some()` if this class is known to be a [`DisjointBase`], or `None` if it is not.
    pub(super) fn as_disjoint_base(self, db: &'db dyn Db) -> Option<DisjointBase<'db>> {
        if self
            .known_function_decorators(db)
            .contains(&KnownFunction::DisjointBase)
            && !self.is_typed_dict(db)
            && !self.is_protocol(db)
        {
            Some(DisjointBase::due_to_decorator(self))
        } else if SlotsKind::from(db, self) == SlotsKind::NotEmpty {
            Some(DisjointBase::due_to_dunder_slots(ClassLiteral::Static(
                self,
            )))
        } else {
            None
        }
    }

    /// Iterate over this class's explicit bases, resolving them in the same way as MRO
    /// construction, filtering out any bases that are not fully static class objects.
    fn fully_static_explicit_bases(self, db: &'db dyn Db) -> impl Iterator<Item = ClassType<'db>> {
        self.explicit_bases(db)
            .iter()
            .copied()
            .filter_map(move |ty| {
                ClassBase::try_from_type(db, ty, Some(ClassLiteral::Static(self)))
                    .and_then(ClassBase::into_class)
            })
    }

    /// Determine if this class is a protocol.
    ///
    /// This method relies on the accuracy of the [`KnownClass::is_protocol`] method,
    /// which hardcodes knowledge about certain special-cased classes. See the docs on
    /// that method for why we do this rather than relying on generalised logic for all
    /// classes, including the special-cased ones that are included in the [`KnownClass`]
    /// enum.
    pub(crate) fn is_protocol(self, db: &'db dyn Db) -> bool {
        self.known(db)
            .map(KnownClass::is_protocol)
            .unwrap_or_else(|| {
                // Iterate through the last three bases of the class
                // searching for `Protocol` or `Protocol[]` in the bases list.
                //
                // If `Protocol` is present in the bases list of a valid protocol class, it must either:
                //
                // - be the last base
                // - OR be the last-but-one base (with the final base being `Generic[]` or `object`)
                // - OR be the last-but-two base (with the penultimate base being `Generic[]`
                //                                and the final base being `object`)
                self.explicit_bases(db).iter().rev().take(3).any(|base| {
                    matches!(
                        base,
                        Type::SpecialForm(SpecialFormType::Protocol)
                            | Type::KnownInstance(KnownInstanceType::SubscriptedProtocol(_))
                    )
                })
            })
    }

    /// Return the types of the decorators on this class
    fn decorators(self, db: &'db dyn Db) -> &'db [Type<'db>] {
        if !self.has_decorators(db) {
            return &[];
        }
        self.decorators_inner(db)
    }

    #[salsa::tracked(returns(deref), cycle_initial=|_, _, _| Box::default(), heap_size=ruff_memory_usage::heap_size)]
    fn decorators_inner(self, db: &'db dyn Db) -> Box<[Type<'db>]> {
        tracing::trace!("StaticClassLiteral::decorators: {}", self.name(db));

        let module = parsed_module(db, self.file(db)).load(db);

        let class_stmt = self.node(db, &module);
        if class_stmt.decorator_list.is_empty() {
            return Box::new([]);
        }

        let class_definition =
            semantic_index(db, self.file(db)).expect_single_definition(class_stmt);

        class_stmt
            .decorator_list
            .iter()
            .map(|decorator_node| {
                definition_expression_type(db, class_definition, &decorator_node.expression)
            })
            .collect()
    }

    pub(crate) fn known_function_decorators(
        self,
        db: &'db dyn Db,
    ) -> impl Iterator<Item = KnownFunction> + 'db {
        self.decorators(db)
            .iter()
            .filter_map(|deco| deco.as_function_literal())
            .filter_map(|decorator| decorator.known(db))
    }

    /// Iterate through the decorators on this class, returning the index of the first one
    /// that is either `@dataclass` or `@dataclass(...)`.
    pub(crate) fn find_dataclass_decorator_position(self, db: &'db dyn Db) -> Option<usize> {
        let module = parsed_module(db, self.file(db)).load(db);
        let class_stmt = self.node(db, &module);
        let class_definition =
            semantic_index(db, self.file(db)).expect_single_definition(class_stmt);

        class_stmt.decorator_list.iter().position(|decorator| {
            let decorator_callable = decorator
                .expression
                .as_call_expr()
                .map_or(&decorator.expression, |call| &call.func);

            definition_expression_type(db, class_definition, decorator_callable)
                .as_function_literal()
                .is_some_and(|function| function.is_known(db, KnownFunction::Dataclass))
        })
    }

    /// Is this class final?
    pub(crate) fn is_final(self, db: &'db dyn Db) -> bool {
        self.known_function_decorators(db)
            .contains(&KnownFunction::Final)
            || enum_metadata(db, ClassLiteral::Static(self)).is_some()
    }

    /// Attempt to resolve the [method resolution order] ("MRO") for this class.
    /// If the MRO is unresolvable, return an error indicating why the class's MRO
    /// cannot be accurately determined. The error returned contains a fallback MRO
    /// that will be used instead for the purposes of type inference.
    ///
    /// The MRO is the tuple of classes that can be retrieved as the `__mro__`
    /// attribute on a class at runtime.
    ///
    /// [method resolution order]: https://docs.python.org/3/glossary.html#term-method-resolution-order
    #[salsa::tracked(
        returns(as_ref),
        cycle_initial=|db, _, self_: StaticClassLiteral<'db>, specialization| {
            Err(StaticMroError::cycle(
                db,
                self_.apply_optional_specialization(db, specialization),
            ))
        },
        heap_size=ruff_memory_usage::heap_size
    )]
    pub(crate) fn try_mro(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
    ) -> Result<Mro<'db>, StaticMroError<'db>> {
        tracing::trace!("StaticClassLiteral::try_mro: {}", self.name(db));
        Mro::of_static_class(db, self, specialization)
    }

    /// Iterate over the [method resolution order] ("MRO") of the class.
    ///
    /// If the MRO could not be accurately resolved, this method falls back to iterating
    /// over an MRO that has the class directly inheriting from `Unknown`. Use
    /// [`StaticClassLiteral::try_mro`] if you need to distinguish between the success and failure
    /// cases rather than simply iterating over the inferred resolution order for the class.
    ///
    /// [method resolution order]: https://docs.python.org/3/glossary.html#term-method-resolution-order
    pub(crate) fn iter_mro(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
    ) -> MroIterator<'db> {
        MroIterator::new(db, ClassLiteral::Static(self), specialization)
    }

    /// Return `true` if `other` is present in this class's MRO.
    pub(super) fn is_subclass_of(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        other: ClassType<'db>,
    ) -> bool {
        // `is_subclass_of` is checking the subtype relation, in which gradual types do not
        // participate, so we should not return `True` if we find `Any/Unknown` in the MRO.
        self.iter_mro(db, specialization)
            .contains(&ClassBase::Class(other))
    }

    /// Return the properties that affect how instances of this class are represented.
    pub(super) fn instance_flags(self, db: &'db dyn Db) -> ClassInstanceFlags {
        #[salsa::tracked(
            returns(copy),
            cycle_initial=|_, _, _| ClassInstanceFlags::empty(),
            heap_size=ruff_memory_usage::heap_size,
        )]
        fn instance_flags_inner<'db>(
            db: &'db dyn Db,
            class: StaticClassLiteral<'db>,
        ) -> ClassInstanceFlags {
            let mut flags = ClassInstanceFlags::empty();
            for base in class.iter_mro(db, None) {
                if base.is_typed_dict() {
                    flags.insert(ClassInstanceFlags::TYPED_DICT);
                }
                if base.is_explicit_any_base() {
                    flags.insert(ClassInstanceFlags::INHERITS_FROM_EXPLICIT_ANY);
                }
            }
            flags
        }

        if let Some(known) = self.known(db) {
            return if known.is_typed_dict_subclass() {
                ClassInstanceFlags::TYPED_DICT
            } else {
                ClassInstanceFlags::empty()
            };
        }

        if !self.has_explicit_bases(db) {
            return ClassInstanceFlags::empty();
        }
        instance_flags_inner(db, self)
    }

    /// Return the module defining the `TypedDict` base of this class.
    #[salsa::tracked(returns(copy), cycle_initial=|_, _, _| None, heap_size=ruff_memory_usage::heap_size)]
    pub(crate) fn typed_dict_module(self, db: &'db dyn Db) -> Option<TypedDictModule> {
        self.iter_mro(db, None)
            .find_map(ClassBase::typed_dict_module)
    }

    /// Return `true` if this class constitutes a typed dict specification (inherits from
    /// `typing.TypedDict` or `typing_extensions.TypedDict`, either directly or indirectly).
    pub fn is_typed_dict(self, db: &'db dyn Db) -> bool {
        self.instance_flags(db)
            .contains(ClassInstanceFlags::TYPED_DICT)
    }

    /// Return `true` if this class is, or inherits from, a `NamedTuple` (inherits from
    /// `typing.NamedTuple`, either directly or indirectly, including functional forms like
    /// `NamedTuple("X", ...)`).
    pub(crate) fn has_named_tuple_class_in_mro(self, db: &'db dyn Db) -> bool {
        self.iter_mro(db, None)
            .filter_map(ClassBase::into_class)
            .any(|base| match base.class_literal(db) {
                ClassLiteral::DynamicNamedTuple(_) => true,
                ClassLiteral::Dynamic(_)
                | ClassLiteral::DynamicTypedDict(_)
                | ClassLiteral::DynamicEnum(_) => false,
                ClassLiteral::Static(class) => class
                    .explicit_bases(db)
                    .contains(&Type::SpecialForm(SpecialFormType::NamedTuple)),
            })
    }

    /// Compute `TypedDict` parameters dynamically based on MRO detection and AST parsing.
    fn typed_dict_params(self, db: &'db dyn Db) -> Option<TypedDictParams> {
        if !self.is_typed_dict(db) {
            return None;
        }

        let module = parsed_module(db, self.file(db)).load(db);
        let class_stmt = self.node(db, &module);
        Some(typed_dict_params_from_class_def(class_stmt))
    }

    /// Returns dataclass params for this class, sourced from both dataclass params and dataclass
    /// transform params
    fn merged_dataclass_params(
        self,
        db: &'db dyn Db,
        field_policy: CodeGeneratorKind<'db>,
    ) -> (Option<DataclassParams<'db>>, Option<DataclassParams<'db>>) {
        let dataclass_params = self.dataclass_params(db);

        let mut transformer_params =
            field_policy
                .dataclass_transformer_params()
                .map(|transformer_params| {
                    DataclassParams::from_transformer_params(db, transformer_params)
                });

        // Dataclass transformer flags can be overwritten using class arguments.
        if let Some(transformer_params) = transformer_params.as_mut() {
            if let Some(class_def) = self.definition(db).kind(db).as_class() {
                let module = parsed_module(db, self.file(db)).load(db);

                if let Some(arguments) = &class_def.node(&module).arguments {
                    let mut flags = transformer_params.flags(db);

                    for keyword in &arguments.keywords {
                        if let Some(arg_name) = &keyword.arg {
                            if let Some(is_set) =
                                keyword.value.as_boolean_literal_expr().map(|b| b.value)
                            {
                                for (flag_name, flag) in DATACLASS_FLAGS {
                                    if arg_name.as_str() == *flag_name {
                                        flags.set(*flag, is_set);
                                    }
                                }
                            }
                        }
                    }

                    *transformer_params =
                        DataclassParams::new(db, flags, transformer_params.field_specifiers(db));
                }
            }
        }

        (dataclass_params, transformer_params)
    }

    /// Returns the effective frozen status of this class if it's a dataclass-like class.
    ///
    /// Returns `Some(true)` for a frozen dataclass-like class, `Some(false)` for a non-frozen one,
    /// and `None` if the class is not a dataclass-like class, or if the dataclass is neither frozen
    /// nor non-frozen.
    pub(crate) fn is_frozen_dataclass(self, db: &'db dyn Db) -> Option<bool> {
        // Check if this is a base-class-based transformer that has dataclass_transformer_params directly
        // attached to it (because it is itself decorated with `@dataclass_transform`), or if this class
        // has an explicit metaclass that is decorated with `@dataclass_transform`.
        //
        // In both cases, this signifies that this class is neither frozen nor non-frozen.
        //
        // See <https://typing.python.org/en/latest/spec/dataclasses.html#dataclass-semantics> for details.
        if self.dataclass_transformer_params(db).is_some()
            || self
                .try_metaclass(db)
                .is_ok_and(|(_, info)| info.is_some_and(|i| i.from_explicit_metaclass))
        {
            return None;
        }

        if let field_policy @ CodeGeneratorKind::DataclassLike(_) =
            CodeGeneratorKind::from_class(db, self.into())?
        {
            // Otherwise, if this class is a dataclass-like class, determine its frozen status based on
            // dataclass params and dataclass transformer params.
            Some(self.has_dataclass_param(db, field_policy, DataclassFlags::FROZEN))
        } else {
            None
        }
    }

    /// Return `true` if Pydantic's effective model configuration marks this model as frozen.
    fn is_frozen_pydantic_model(db: &'db dyn Db, field_policy: CodeGeneratorKind<'db>) -> bool {
        field_policy
            .pydantic_metadata()
            .is_some_and(|metadata| metadata.is_frozen(db))
    }

    /// Checks if the given dataclass parameter flag is set for this class.
    /// This checks both the `dataclass_params` and `transformer_params`.
    pub(crate) fn has_dataclass_param(
        self,
        db: &'db dyn Db,
        field_policy: CodeGeneratorKind<'db>,
        param: DataclassFlags,
    ) -> bool {
        let (dataclass_params, transformer_params) = self.merged_dataclass_params(db, field_policy);
        dataclass_params.is_some_and(|params| params.flags(db).contains(param))
            || transformer_params.is_some_and(|params| params.flags(db).contains(param))
    }

    /// Returns the nearest `@dataclass_transform` parameters for this class or its MRO.
    ///
    /// This is used for metaclass-based transforms because `__dataclass_transform__` is inherited,
    /// so a metaclass subclass should preserve the transform metadata of its decorated base class
    /// unless it provides its own.
    fn inherited_dataclass_transformer_params(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
    ) -> Option<DataclassTransformerParams<'db>> {
        self.dataclass_transformer_params(db).or_else(|| {
            self.iter_mro(db, specialization).skip(1).find_map(|base| {
                base.into_class().and_then(|class| {
                    class
                        .static_class_literal(db)
                        .and_then(|(lit, _)| lit.dataclass_transformer_params(db))
                })
            })
        })
    }

    /// Return the explicit `metaclass` of this class, if one is defined.
    ///
    /// ## Note
    /// Only call this function from queries in the same file or your
    /// query depends on the AST of another file (bad!).
    fn explicit_metaclass(self, db: &'db dyn Db, module: &ParsedModuleRef) -> Option<Type<'db>> {
        let class_stmt = self.node(db, module);
        let metaclass_node = &class_stmt
            .arguments
            .as_ref()?
            .find_keyword("metaclass")?
            .value;

        let class_definition = self.definition(db);

        Some(definition_expression_type(
            db,
            class_definition,
            metaclass_node,
        ))
    }

    /// Return the metaclass of this class, or `type[Unknown]` if the metaclass cannot be inferred.
    pub(crate) fn metaclass(self, db: &'db dyn Db) -> Type<'db> {
        self.try_metaclass(db)
            .map(|(ty, _)| ty)
            .unwrap_or_else(|_| SubclassOfType::subclass_of_unknown())
    }

    /// Return the metaclass of this class, or an error if the metaclass cannot be inferred.
    pub(in crate::types) fn try_metaclass(
        self,
        db: &'db dyn Db,
    ) -> Result<(Type<'db>, Option<MetaclassTransformInfo<'db>>), MetaclassError<'db>> {
        #[salsa::tracked(
            returns(clone),
            cycle_initial=|_, _, _| Err(MetaclassError {
                kind: MetaclassErrorKind::Cycle,
            }),
            heap_size=ruff_memory_usage::heap_size,
        )]
        fn try_metaclass_inner<'db>(
            db: &'db dyn Db,
            class: StaticClassLiteral<'db>,
        ) -> Result<(Type<'db>, Option<MetaclassTransformInfo<'db>>), MetaclassError<'db>> {
            tracing::trace!("StaticClassLiteral::try_metaclass: {}", class.name(db));

            // Identify the class's own metaclass (or take the first base class's metaclass).
            let mut base_classes = class.fully_static_explicit_bases(db).peekable();

            if base_classes.peek().is_some() && class.inheritance_cycle(db).is_some() {
                // We emit diagnostics for cyclic class definitions elsewhere.
                // Avoid attempting to infer the metaclass if the class is cyclically defined.
                return Ok((SubclassOfType::subclass_of_unknown(), None));
            }

            if class.try_mro(db, None).is_err_and(StaticMroError::is_cycle) {
                return Ok((SubclassOfType::subclass_of_unknown(), None));
            }

            let module = parsed_module(db, class.file(db)).load(db);

            let explicit_metaclass = class.explicit_metaclass(db, &module);

            // Generic metaclasses parameterized by type variables are not supported.
            // `metaclass=Meta[int]` is fine, but `metaclass=Meta[T]` is not.
            // See: https://typing.python.org/en/latest/spec/generics.html#generic-metaclasses
            if let Some(Type::GenericAlias(alias)) = explicit_metaclass {
                let specialization_has_typevars = alias
                    .specialization(db)
                    .types(db)
                    .iter()
                    .any(|ty| ty.has_typevar_or_typevar_instance(db));
                if specialization_has_typevars {
                    return Err(MetaclassError {
                        kind: MetaclassErrorKind::GenericMetaclass,
                    });
                }
            }

            let (metaclass, class_metaclass_was_from) = if let Some(metaclass) = explicit_metaclass
            {
                (metaclass, class)
            } else if let Some(base_class) = base_classes.next() {
                // For dynamic classes, we can't get a StaticClassLiteral, so use this class for
                // tracking.
                let base_class_literal = base_class
                    .static_class_literal(db)
                    .map(|(lit, _)| lit)
                    .unwrap_or(class);
                (base_class.metaclass(db), base_class_literal)
            } else {
                (KnownClass::Type.to_class_literal(db), class)
            };

            let mut candidate = if let Some(metaclass_ty) = metaclass.to_class_type(db) {
                MetaclassCandidate {
                    metaclass: metaclass_ty,
                    explicit_metaclass_of: class_metaclass_was_from,
                }
            } else {
                let name = Type::string_literal(db, class.name(db));
                let bases = Type::heterogeneous_tuple(db, class.explicit_bases(db));
                let namespace = KnownClass::Dict
                    .to_specialized_instance(db, &[KnownClass::Str.to_instance(db), Type::any()]);

                // TODO: Other keyword arguments?
                let arguments = CallArguments::positional([name, bases, namespace]);

                let return_ty_result = match metaclass.try_call(db, &arguments) {
                    Ok(bindings) => Ok(bindings.return_type(db)),

                    Err(CallError(CallErrorKind::NotCallable, bindings)) => Err(MetaclassError {
                        kind: MetaclassErrorKind::NotCallable(bindings.callable_type()),
                    }),

                    // TODO we should also check for binding errors that would indicate the metaclass
                    // does not accept the right arguments
                    Err(CallError(CallErrorKind::BindingError, bindings)) => {
                        Ok(bindings.return_type(db))
                    }

                    Err(CallError(CallErrorKind::PossiblyNotCallable, _)) => Err(MetaclassError {
                        kind: MetaclassErrorKind::PartlyNotCallable(metaclass),
                    }),
                };

                return return_ty_result.map(|ty| (ty.to_meta_type(db), None));
            };

            // Reconcile all base classes' metaclasses with the candidate metaclass.
            //
            // See:
            // - https://docs.python.org/3/reference/datamodel.html#determining-the-appropriate-metaclass
            // - https://github.com/python/cpython/blob/83ba8c2bba834c0b92de669cac16fcda17485e0e/Objects/typeobject.c#L3629-L3663
            for base_class in base_classes {
                let metaclass = base_class.metaclass(db);
                let Some(metaclass) = metaclass.to_class_type(db) else {
                    continue;
                };
                // For dynamic classes, we can't get a StaticClassLiteral, so use this class for
                // tracking.
                let base_class_literal = base_class
                    .static_class_literal(db)
                    .map(|(lit, _)| lit)
                    .unwrap_or(class);
                if metaclass.is_subclass_of(db, candidate.metaclass) {
                    candidate = MetaclassCandidate {
                        metaclass,
                        explicit_metaclass_of: base_class_literal,
                    };
                    continue;
                }
                if candidate.metaclass.is_subclass_of(db, metaclass) {
                    continue;
                }
                return Err(MetaclassError {
                    kind: MetaclassErrorKind::Conflict {
                        candidate1: candidate,
                        candidate2: MetaclassCandidate {
                            metaclass,
                            explicit_metaclass_of: base_class_literal,
                        },
                        candidate1_is_base_class: explicit_metaclass.is_none(),
                    },
                });
            }

            let transform_info = candidate
                .metaclass
                .static_class_literal(db)
                .and_then(|(metaclass_literal, specialization)| {
                    metaclass_literal.inherited_dataclass_transformer_params(db, specialization)
                })
                .map(|params| MetaclassTransformInfo {
                    params,
                    from_explicit_metaclass: candidate.explicit_metaclass_of == class,
                });
            Ok((candidate.metaclass.into(), transform_info))
        }

        if !self.has_explicit_bases(db) && !self.has_explicit_metaclass(db) {
            return Ok((KnownClass::Type.to_class_literal(db), None));
        }
        try_metaclass_inner(db, self)
    }

    /// Returns the class member of this class named `name`.
    ///
    /// The member resolves to a member on the class itself or any of its proper superclasses.
    ///
    /// TODO: Should this be made private...?
    pub(super) fn class_member(
        self,
        db: &'db dyn Db,
        name: &str,
        policy: MemberLookupPolicy,
    ) -> PlaceAndQualifiers<'db> {
        self.class_member_inner(db, None, name, policy)
    }

    pub(super) fn class_member_inner(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        name: &str,
        policy: MemberLookupPolicy,
    ) -> PlaceAndQualifiers<'db> {
        self.class_member_from_mro(db, name, policy, self.iter_mro(db, specialization))
    }

    pub(crate) fn class_member_from_mro(
        self,
        db: &'db dyn Db,
        name: &str,
        policy: MemberLookupPolicy,
        mro_iter: impl Iterator<Item = ClassBase<'db>>,
    ) -> PlaceAndQualifiers<'db> {
        fn into_function_like_callable<'d>(db: &'d dyn Db, ty: Type<'d>) -> Type<'d> {
            match ty {
                Type::Callable(callable_ty)
                    if callable_ty.is_regular(db)
                        && callable_ty.signatures(db).has_parameters() =>
                {
                    Type::Callable(callable_ty.into_function_like(db))
                }
                Type::Union(union) => {
                    union.map(db, |element| into_function_like_callable(db, *element))
                }
                Type::Intersection(intersection) => intersection
                    .map_positive(db, |element| into_function_like_callable(db, *element)),
                _ => ty,
            }
        }

        let result = MroLookup::new(db, mro_iter).class_member(
            name,
            policy,
            self.inherited_generic_context(db),
            self.is_known(db, KnownClass::Object),
        );

        let mut member = match result {
            ClassMemberResult::Done(result) => result.finalize(db),
            ClassMemberResult::TypedDict(module) => {
                typed_dict_class_member(db, self.identity_specialization(db), module, policy, name)
            }
        };

        // We generally treat dunder attributes with `Callable` types as function-like callables.
        // See `callables_as_descriptors.md` for more details.
        if name.starts_with("__") && name.ends_with("__") {
            member = member.map_type(|ty| into_function_like_callable(db, ty));
        }

        member
    }

    #[salsa::tracked(
        returns(ref),
        cycle_initial=|_, _, _| PluginClassTransformPatch::default(),
        heap_size=get_size2::GetSize::get_heap_size
    )]
    fn plugin_class_transform_patch(self, db: &'db dyn Db) -> PluginClassTransformPatch<'db> {
        let semantic_plugins = Program::get(db).semantic_plugins(db);
        if semantic_plugins.is_empty() {
            return PluginClassTransformPatch::default();
        }

        let route_candidates = plugin_class_transform_route_candidates(db, self);
        if route_candidates.is_empty() {
            return PluginClassTransformPatch::default();
        }

        let mut matching_plugins = semantic_plugins
            .plugins()
            .iter()
            .filter(|plugin| {
                plugin
                    .class_transform_claims()
                    .iter()
                    .any(|claim| route_candidates.iter().any(|candidate| candidate == claim))
            })
            .peekable();

        if matching_plugins.peek().is_none() {
            return PluginClassTransformPatch::default();
        }

        let class_summary = plugin_class_summary(db, self);
        let mut fields = Vec::new();
        let mut class_members = Vec::new();
        let mut instance_members = Vec::new();
        let mut constructor = None;

        for plugin in matching_plugins {
            tracing::trace!(
                plugin_id = plugin.id(),
                runtime = ?plugin.runtime(),
                class = %ClassLiteral::Static(self).qualified_name(db),
                "executing class-transform plugin"
            );
            let request = plugin_analyze_class_request(
                db,
                self,
                &class_summary,
                plugin_project_index_json(db, plugin),
            );
            let virtual_types = plugin_project_index_virtual_types(db, plugin);
            let response = execute_class_transform_plugin(db, plugin, &request);
            merge_plugin_class_response(
                db,
                self,
                response,
                virtual_types,
                &mut fields,
                &mut class_members,
                &mut instance_members,
                &mut constructor,
            );
        }

        PluginClassTransformPatch {
            fields: fields.into_boxed_slice(),
            class_members: class_members.into_boxed_slice(),
            instance_members: instance_members.into_boxed_slice(),
            constructor,
        }
    }

    #[salsa::tracked(
        returns(as_ref),
        cycle_initial=|_, _, _, _| None,
        heap_size=ruff_memory_usage::heap_size,
    )]
    fn plugin_class_member_patch(
        self,
        db: &'db dyn Db,
        name: Name,
    ) -> Option<PluginMemberPatch<'db>> {
        self.plugin_member_patch(db, &name, PluginMemberScope::Class)
    }

    #[salsa::tracked(
        returns(as_ref),
        cycle_initial=|_, _, _, _| None,
        heap_size=ruff_memory_usage::heap_size,
    )]
    fn plugin_instance_member_patch(
        self,
        db: &'db dyn Db,
        name: Name,
    ) -> Option<PluginMemberPatch<'db>> {
        self.plugin_member_patch(db, &name, PluginMemberScope::Instance)
    }

    fn plugin_member_patch(
        self,
        db: &'db dyn Db,
        name: &Name,
        scope: PluginMemberScope,
    ) -> Option<PluginMemberPatch<'db>> {
        self.plugin_member_patch_with_existing(db, name, scope, None, None)
    }

    fn plugin_member_patch_with_existing(
        self,
        db: &'db dyn Db,
        name: &Name,
        scope: PluginMemberScope,
        existing_ty: Option<Type<'db>>,
        owner_override: Option<Type<'db>>,
    ) -> Option<PluginMemberPatch<'db>> {
        let semantic_plugins = Program::get(db).semantic_plugins(db);
        if semantic_plugins.is_empty() {
            return None;
        }

        let owner_qualified_name = ClassLiteral::Static(self).qualified_name(db).to_string();
        let mut matching_plugins = semantic_plugins
            .plugins()
            .iter()
            .filter(|plugin| {
                let claims = match scope {
                    PluginMemberScope::Class => plugin.class_member_claims(),
                    PluginMemberScope::Instance => plugin.instance_member_claims(),
                };
                let exact = claims.iter().any(|claim| {
                    claim.owner_qualified_name() == owner_qualified_name
                        && claim.member_name() == name.as_str()
                });
                let subclass = scope == PluginMemberScope::Instance
                    && plugin
                        .instance_member_on_subclass_claims()
                        .iter()
                        .any(|claim| {
                            plugin_class_transform_route_candidates(db, self)
                                .iter()
                                .any(|candidate| candidate == claim)
                        });
                exact || subclass
            })
            .collect::<Vec<_>>();

        matching_plugins.sort_by_key(|plugin| plugin.id());
        if matching_plugins.is_empty() {
            return None;
        }

        let mut resolved_member = None;

        for plugin in matching_plugins {
            tracing::trace!(
                plugin_id = plugin.id(),
                class = %owner_qualified_name,
                member = name.as_str(),
                ?scope,
                runtime = ?plugin.runtime(),
                "executing member plugin"
            );
            let request = plugin_resolve_member_request(
                db,
                self,
                name.as_str(),
                scope,
                existing_ty,
                owner_override,
                plugin_project_index_json(db, plugin),
            );
            let candidate = plugin_member_response_to_patch(
                db,
                execute_member_plugin(db, plugin, &request),
                name,
                plugin_project_index_virtual_types(db, plugin),
            );
            if candidate.is_some() && resolved_member.is_some() {
                tracing::warn!(
                    plugin_id = plugin.id(),
                    class = %owner_qualified_name,
                    member = name.as_str(),
                    ?scope,
                    "multiple plugins replaced the same member; keeping the lexicographically first plugin"
                );
            } else if candidate.is_some() {
                resolved_member = candidate;
            }
        }

        resolved_member
    }

    fn own_plugin_class_transform_member(self, db: &'db dyn Db, name: &str) -> Option<Member<'db>> {
        let patch = self.plugin_class_transform_patch(db);

        patch
            .class_members
            .iter()
            .find(|member| member.name.as_str() == name)
            .map(plugin_member_to_member)
            .or_else(|| {
                patch
                    .fields
                    .iter()
                    .find(|field| field.name.as_str() == name)
                    .and_then(|field| field.descriptor_class_ty)
                    .map(Member::definitely_declared)
            })
    }

    fn own_plugin_dynamic_class_member(self, db: &'db dyn Db, name: &str) -> Option<Member<'db>> {
        self.plugin_class_member_patch(db, Name::new(name))
            .map(plugin_member_to_member)
            .or_else(|| self.own_plugin_contributed_member(db, name, PluginMemberScope::Class))
    }

    fn own_plugin_class_transform_instance_member(
        self,
        db: &'db dyn Db,
        name: &str,
    ) -> Option<Member<'db>> {
        let patch = self.plugin_class_transform_patch(db);

        if let Some(member) = patch
            .instance_members
            .iter()
            .find(|member| member.name.as_str() == name)
        {
            return Some(plugin_member_to_member(member));
        }

        patch
            .fields
            .iter()
            .find(|field| field.name.as_str() == name)
            .map(|field| Member::definitely_declared(field.instance_get_ty))
    }

    fn own_plugin_replacement_instance_member(
        self,
        db: &'db dyn Db,
        name: &str,
        existing_ty: Option<Type<'db>>,
    ) -> Option<Member<'db>> {
        let patch = self.plugin_class_transform_patch(db);

        patch
            .instance_members
            .iter()
            .find(|member| member.name.as_str() == name && member.replace_existing)
            .map(plugin_member_to_member)
            .or_else(|| {
                patch
                    .fields
                    .iter()
                    .find(|field| field.name.as_str() == name && field.replace_existing)
                    .map(|field| Member::definitely_declared(field.instance_get_ty))
            })
            .or_else(|| {
                self.plugin_member_patch_with_existing(
                    db,
                    &Name::new(name),
                    PluginMemberScope::Instance,
                    existing_ty,
                    None,
                )
                .filter(|member| member.replace_existing)
                .map(|member| plugin_member_to_member(&member))
            })
            .or_else(|| {
                self.own_plugin_contributed_member_patch(db, name, PluginMemberScope::Instance)
                    .filter(|patch| patch.replaces_existing())
                    .and_then(|patch| {
                        plugin_contribution_to_member(patch, PluginMemberScope::Instance)
                    })
            })
    }

    pub(super) fn plugin_replacement_instance_member(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        name: &str,
        existing_ty: Option<Type<'db>>,
    ) -> Option<PlaceAndQualifiers<'db>> {
        if self.is_typed_dict(db) {
            return None;
        }

        for superclass in self.iter_mro(db, specialization) {
            let ClassBase::Class(class) = superclass else {
                continue;
            };

            let member = match class {
                ClassType::NonGeneric(ClassLiteral::Static(class)) => {
                    let Some(member) =
                        class.own_plugin_replacement_instance_member(db, name, existing_ty)
                    else {
                        continue;
                    };
                    member.inner
                }
                ClassType::Generic(generic) => {
                    let Some(member) = generic.origin(db).own_plugin_replacement_instance_member(
                        db,
                        name,
                        existing_ty,
                    ) else {
                        continue;
                    };
                    member.inner.map_type(|ty| {
                        ty.apply_optional_specialization(db, Some(generic.specialization(db)))
                    })
                }
                ClassType::NonGeneric(
                    ClassLiteral::Dynamic(_)
                    | ClassLiteral::DynamicNamedTuple(_)
                    | ClassLiteral::DynamicTypedDict(_)
                    | ClassLiteral::DynamicEnum(_),
                ) => continue,
            };

            return Some(member);
        }

        None
    }

    pub(super) fn plugin_annotated_instance_member(
        self,
        db: &'db dyn Db,
        name: &str,
        owner: Type<'db>,
    ) -> Option<PlaceAndQualifiers<'db>> {
        self.plugin_member_patch_with_existing(
            db,
            &Name::new(name),
            PluginMemberScope::Instance,
            None,
            Some(owner),
        )
        .map(|member| plugin_member_to_member(&member).inner)
    }

    pub(super) fn own_plugin_class_transform_instance_assignment_member(
        self,
        db: &'db dyn Db,
        name: &str,
    ) -> Option<Member<'db>> {
        self.plugin_class_transform_patch(db)
            .fields
            .iter()
            .find(|field| field.name.as_str() == name)
            .and_then(|field| field.instance_set_ty)
            .map(Member::definitely_declared)
    }

    fn own_plugin_contributed_instance_assignment_member(
        self,
        db: &'db dyn Db,
        name: &str,
    ) -> Option<Member<'db>> {
        let semantic_plugins = Program::get(db).semantic_plugins(db);
        if semantic_plugins.is_empty() {
            return None;
        }

        let owner_qualified_name = ClassLiteral::Static(self).qualified_name(db).to_string();
        semantic_plugins
            .plugins()
            .iter()
            .filter(|plugin| plugin.project_index_enabled())
            .find_map(|plugin| {
                plugin_project_index(db, SemanticPluginId::new(db, plugin.id().to_string()))
                    .contributions
                    .iter()
                    .find_map(|contribution| {
                        if !contribution
                            .target
                            .matches(PluginMemberScope::Instance, &owner_qualified_name)
                        {
                            return None;
                        }
                        let PluginContributionMemberPatch::Field(field) = &contribution.patch
                        else {
                            return None;
                        };
                        if field.name.as_str() != name {
                            return None;
                        }
                        field.instance_set_ty.map(Member::definitely_declared)
                    })
            })
    }

    pub(super) fn plugin_class_transform_instance_assignment_member(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        name: &str,
    ) -> Option<PlaceAndQualifiers<'db>> {
        if self.is_typed_dict(db) {
            return None;
        }

        for superclass in self.iter_mro(db, specialization) {
            let ClassBase::Class(class) = superclass else {
                continue;
            };

            let member = match class {
                ClassType::NonGeneric(ClassLiteral::Static(class)) => {
                    class
                        .own_plugin_class_transform_instance_assignment_member(db, name)?
                        .inner
                }
                ClassType::Generic(generic) => generic
                    .origin(db)
                    .own_plugin_class_transform_instance_assignment_member(db, name)?
                    .inner
                    .map_type(|ty| {
                        ty.apply_optional_specialization(db, Some(generic.specialization(db)))
                    }),
                ClassType::NonGeneric(
                    ClassLiteral::Dynamic(_)
                    | ClassLiteral::DynamicNamedTuple(_)
                    | ClassLiteral::DynamicTypedDict(_)
                    | ClassLiteral::DynamicEnum(_),
                ) => continue,
            };

            return Some(member);
        }

        None
    }

    pub(super) fn plugin_contributed_instance_assignment_member(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        name: &str,
    ) -> Option<PlaceAndQualifiers<'db>> {
        if self.is_typed_dict(db) {
            return None;
        }

        for superclass in self.iter_mro(db, specialization) {
            let ClassBase::Class(class) = superclass else {
                continue;
            };

            let member = match class {
                ClassType::NonGeneric(ClassLiteral::Static(class)) => {
                    class
                        .own_plugin_contributed_instance_assignment_member(db, name)?
                        .inner
                }
                ClassType::Generic(generic) => generic
                    .origin(db)
                    .own_plugin_contributed_instance_assignment_member(db, name)?
                    .inner
                    .map_type(|ty| {
                        ty.apply_optional_specialization(db, Some(generic.specialization(db)))
                    }),
                ClassType::NonGeneric(
                    ClassLiteral::Dynamic(_)
                    | ClassLiteral::DynamicNamedTuple(_)
                    | ClassLiteral::DynamicTypedDict(_)
                    | ClassLiteral::DynamicEnum(_),
                ) => continue,
            };

            return Some(member);
        }

        None
    }

    fn own_plugin_dynamic_instance_member(
        self,
        db: &'db dyn Db,
        name: &str,
    ) -> Option<Member<'db>> {
        self.plugin_instance_member_patch(db, Name::new(name))
            .map(plugin_member_to_member)
    }

    fn own_plugin_instance_member_after_miss(
        self,
        db: &'db dyn Db,
        name: &str,
    ) -> Option<Member<'db>> {
        self.own_plugin_class_transform_instance_member(db, name)
            .or_else(|| self.own_plugin_dynamic_instance_member(db, name))
            .or_else(|| self.own_plugin_contributed_member(db, name, PluginMemberScope::Instance))
    }

    fn own_plugin_contributed_member(
        self,
        db: &'db dyn Db,
        name: &str,
        scope: PluginMemberScope,
    ) -> Option<Member<'db>> {
        self.own_plugin_contributed_member_patch(db, name, scope)
            .and_then(|patch| plugin_contribution_to_member(patch, scope))
    }

    fn own_plugin_contributed_member_patch(
        self,
        db: &'db dyn Db,
        name: &str,
        scope: PluginMemberScope,
    ) -> Option<&'db PluginContributionMemberPatch<'db>> {
        let semantic_plugins = Program::get(db).semantic_plugins(db);
        if semantic_plugins.is_empty() {
            return None;
        }

        let owner_qualified_name = ClassLiteral::Static(self).qualified_name(db).to_string();
        let patch = semantic_plugins
            .plugins()
            .iter()
            .filter(|plugin| plugin.project_index_enabled())
            .find_map(|plugin| {
                plugin_project_index(db, SemanticPluginId::new(db, plugin.id().to_string()))
                    .contributions
                    .iter()
                    .find(|contribution| {
                        contribution.target.matches(scope, &owner_qualified_name)
                            && contribution
                                .patch
                                .member_name()
                                .is_some_and(|member_name| member_name.as_str() == name)
                    })
                    .map(|contribution| &contribution.patch)
            });
        patch
    }

    fn plugin_contributed_constructor_patch(
        self,
        db: &'db dyn Db,
    ) -> Option<PluginConstructorPatch<'db>> {
        let semantic_plugins = Program::get(db).semantic_plugins(db);
        if semantic_plugins.is_empty() {
            return None;
        }

        let owner_qualified_name = ClassLiteral::Static(self).qualified_name(db).to_string();
        semantic_plugins
            .plugins()
            .iter()
            .filter(|plugin| plugin.project_index_enabled())
            .find_map(|plugin| {
                plugin_project_index(db, SemanticPluginId::new(db, plugin.id().to_string()))
                    .contributions
                    .iter()
                    .find_map(|contribution| {
                        let PluginContributionTarget::Constructor(qualified_name) =
                            &contribution.target
                        else {
                            return None;
                        };
                        if qualified_name != &owner_qualified_name {
                            return None;
                        }
                        let PluginContributionMemberPatch::Constructor(constructor) =
                            &contribution.patch
                        else {
                            return None;
                        };
                        Some(constructor.clone())
                    })
            })
    }

    fn is_own_plugin_instance_field(self, db: &'db dyn Db, name: &str) -> bool {
        self.plugin_class_transform_patch(db)
            .fields
            .iter()
            .any(|field| field.name.as_str() == name)
    }

    fn own_plugin_synthesized_member(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        inherited_generic_context: Option<GenericContext<'db>>,
        name: &str,
    ) -> Option<Type<'db>> {
        if name != "__init__" {
            return None;
        }

        let patch = self.plugin_class_transform_patch(db);
        let contributed_constructor = self.plugin_contributed_constructor_patch(db);
        if patch.constructor.is_none()
            && contributed_constructor.is_none()
            && patch
                .fields
                .iter()
                .all(|field| field.constructor_parameter.is_none())
        {
            return None;
        }

        let instance_ty =
            Type::instance(db, self.apply_optional_specialization(db, specialization));
        let mut parameters = vec![
            Parameter::positional_or_keyword(Name::new_static("self"))
                .with_annotated_type(instance_ty),
        ];

        if let Some(constructor) = patch
            .constructor
            .as_ref()
            .or(contributed_constructor.as_ref())
        {
            parameters.extend(
                constructor
                    .parameters
                    .iter()
                    .filter(|parameter| !is_plugin_self_parameter(parameter))
                    .filter_map(plugin_signature_parameter),
            );
        } else {
            parameters.extend(
                patch
                    .fields
                    .iter()
                    .filter_map(plugin_field_constructor_parameter),
            );
        }

        let signature = Signature::new_generic(
            inherited_generic_context.or_else(|| self.inherited_generic_context(db)),
            Parameters::new(parameters, ParametersKind::Standard),
            Type::none(db),
        );

        Some(Type::function_like_callable(db, signature))
    }

    /// Returns the inferred type of the class member named `name`. Only bound members
    /// or those marked as `ClassVars` are considered.
    ///
    /// Returns [`Place::Undefined`] if `name` cannot be found in this class's scope
    /// directly. Use [`StaticClassLiteral::class_member`] if you require a method that will
    /// traverse through the MRO until it finds the member.
    pub(super) fn own_class_member(
        self,
        db: &'db dyn Db,
        inherited_generic_context: Option<GenericContext<'db>>,
        specialization: Option<Specialization<'db>>,
        name: &str,
    ) -> Member<'db> {
        fn into_dunder_paramspec_callable<'d>(db: &'d dyn Db, ty: Type<'d>) -> Type<'d> {
            match ty {
                Type::Callable(callable_ty)
                    if callable_ty.is_regular(db)
                        && callable_ty.signatures(db).is_single_paramspec().is_some() =>
                {
                    Type::Callable(callable_ty.into_dunder_paramspec(db))
                }
                Type::Union(union) => {
                    union.map(db, |element| into_dunder_paramspec_callable(db, *element))
                }
                Type::Intersection(intersection) => intersection
                    .map_positive(db, |element| into_dunder_paramspec_callable(db, *element)),
                _ => ty,
            }
        }

        // Check if this class is dataclass-like (either via @dataclass or via dataclass_transform)
        if CodeGeneratorKind::from_class(db, self.into())
            .is_some_and(CodeGeneratorKind::is_dataclass_like)
        {
            if name == "__dataclass_fields__" {
                // Make this class look like a subclass of the `DataClassInstance` protocol
                return Member {
                    inner: Place::declared(KnownClass::Dict.to_specialized_instance(
                        db,
                        &[
                            KnownClass::Str.to_instance(db),
                            KnownClass::Field.to_specialized_instance(db, &[Type::any()]),
                        ],
                    ))
                    .with_qualifiers(TypeQualifiers::CLASS_VAR),
                };
            } else if name == "__dataclass_params__" {
                // There is no typeshed class for this. For now, we model it as `Any`.
                return Member {
                    inner: Place::declared(Type::any()).with_qualifiers(TypeQualifiers::CLASS_VAR),
                };
            }
        }

        if CodeGeneratorKind::NamedTuple.matches(db, self.into()) {
            if let Some(field) = self
                .own_fields(db, specialization, CodeGeneratorKind::NamedTuple)
                .get(name)
            {
                let property_getter_signature = Signature::new(
                    Parameters::standard([Parameter::positional_only(Some(Name::new_static(
                        "self",
                    )))]),
                    field.declared_ty,
                );
                let property_getter = Type::single_callable(db, property_getter_signature);
                let property = PropertyInstanceType::new(db, Some(property_getter), None, None);
                return Member::definitely_declared(Type::PropertyInstance(property));
            }
        }

        if let Some(plugin_member) = self.own_plugin_class_transform_member(db, name) {
            return plugin_member;
        }

        let body_scope = self.body_scope(db);
        let member = class_member(db, body_scope, name).map_type(|ty| {
            let ty = if name.starts_with("__") && name.ends_with("__") {
                into_dunder_paramspec_callable(db, ty)
            } else {
                ty
            };

            // The `__new__` and `__init__` members of a non-specialized generic class are handled
            // specially: they inherit the generic context of their class. That lets us treat them
            // as generic functions when constructing the class, and infer the specialization of
            // the class from the arguments that are passed in.
            //
            // We might decide to handle other class methods the same way, having them inherit the
            // class's generic context, and performing type inference on calls to them to determine
            // the specialization of the class. If we do that, we would update this to also apply
            // to any method with a `@classmethod` decorator. (`__init__` would remain a special
            // case, since it's an _instance_ method where we don't yet know the generic class's
            // specialization.)
            match (inherited_generic_context, ty, specialization, name) {
                (
                    Some(generic_context),
                    Type::FunctionLiteral(function),
                    Some(_),
                    "__new__" | "__init__",
                ) => Type::FunctionLiteral(
                    function.with_inherited_generic_context(db, generic_context),
                ),
                _ => ty,
            }
        });

        if member.is_undefined() {
            if let Some(synthesized_member) =
                self.own_synthesized_member(db, specialization, inherited_generic_context, name)
            {
                return Member::definitely_declared(synthesized_member);
            }
            // The symbol was not found in the class scope. It might still be implicitly defined in `@classmethod`s.
            let implicit =
                Self::implicit_attribute(db, body_scope, name, MethodDecorator::ClassMethod);
            return if implicit.is_undefined() {
                self.own_plugin_dynamic_class_member(db, name)
                    .unwrap_or(implicit)
            } else {
                implicit
            };
        }

        // For dataclass-like classes, `KW_ONLY` sentinel fields are not real
        // class attributes; they are markers used by the dataclass decorator to
        // indicate that subsequent fields are keyword-only. Treat them as
        // undefined so the MRO falls through to parent classes.
        if member
            .inner
            .place
            .raw_type()
            .is_some_and(|ty| ty.is_instance_of(db, KnownClass::KwOnly))
            && CodeGeneratorKind::from_static_class(db, self)
                .is_some_and(CodeGeneratorKind::is_dataclass_like)
        {
            return Member::unbound();
        }

        // For enum classes, `nonmember(value)` creates a non-member attribute.
        // At runtime, the enum metaclass unwraps the value, so accessing the attribute
        // returns the inner value, not the `nonmember` wrapper.
        if let Some(ty) = member.inner.place.raw_type() {
            if let Some(value_ty) = try_unwrap_nonmember_value(db, ty) {
                if is_enum_class_by_inheritance(db, self) {
                    return Member::definitely_declared(value_ty);
                }
            }
        }

        member
    }

    /// Returns the type of a synthesized dataclass member like `__init__` or `__lt__`, or
    /// a synthesized `__new__` method for a `NamedTuple`.
    pub(crate) fn own_synthesized_member(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        inherited_generic_context: Option<GenericContext<'db>>,
        name: &str,
    ) -> Option<Type<'db>> {
        // Handle `@functools.total_ordering`: synthesize comparison methods
        // for classes that have `@total_ordering` and define at least one
        // ordering method. The decorator requires at least one of __lt__,
        // __le__, __gt__, or __ge__ to be defined (either in this class or
        // inherited from a superclass, excluding `object`).
        //
        // Only synthesize methods that are not already defined in the MRO.
        // Note: We use direct scope lookups here to avoid infinite recursion
        // through `own_class_member` -> `own_synthesized_member`.
        if self.total_ordering(db)
            && matches!(name, "__lt__" | "__le__" | "__gt__" | "__ge__")
            && !self
                .iter_mro(db, specialization)
                .filter_map(ClassBase::into_class)
                .filter_map(|class| class.static_class_literal(db))
                .filter(|(class, _)| !class.is_known(db, KnownClass::Object))
                .any(|(class, _)| {
                    class_member(db, class.body_scope(db), name)
                        .ignore_possibly_undefined()
                        .is_some()
                })
            && self.has_ordering_method_in_mro(db, specialization)
            && let Some(root_method_ty) = self.total_ordering_root_method(db, specialization)
            && let Some(callables) = root_method_ty.try_upcast_to_callable(db)
        {
            let bool_ty = KnownClass::Bool.to_instance(db);
            let synthesized_callables = callables.map(|callable| {
                let signatures = CallableSignature::from_overloads(
                    callable.signatures(db).iter().map(|signature| {
                        // The generated methods return a union of the root method's return type
                        // and `bool`. This is because `@total_ordering` synthesizes methods like:
                        //     def __gt__(self, other): return not (self == other or self < other)
                        // If `__lt__` returns `int`, then `__gt__` could return `int | bool`.
                        let return_ty =
                            UnionType::from_two_elements(db, signature.return_ty, bool_ty);
                        Signature::new_generic(
                            signature.generic_context,
                            signature.parameters().clone(),
                            return_ty,
                        )
                    }),
                );
                CallableType::new(
                    db,
                    signatures,
                    CallableTypeKind::FunctionLike,
                    CallableFunctionProvenance::None,
                )
            });

            return Some(synthesized_callables.into_type(db));
        }

        // An ordinary subclass of a frozen dataclass is not itself dataclass-like, so the
        // `CodeGeneratorKind::from_class` check below would return `None` before dataclass-like
        // synthesis runs. Still, an instance of such a subclass inherits the frozen dataclass's
        // generated `__setattr__`, which rejects writes to frozen base fields.
        if name == "__setattr__"
            && let Some(synthesized_setattr) =
                self.own_frozen_dataclass_subclass_setattr(db, specialization)
        {
            return Some(synthesized_setattr);
        }

        if let Some(plugin_member) =
            self.own_plugin_synthesized_member(db, specialization, inherited_generic_context, name)
        {
            return Some(plugin_member);
        }

        let field_policy = CodeGeneratorKind::from_class(db, self.into())?;
        let pydantic_constructor_fields_are_keyword_only =
            field_policy.is_pydantic() && pydantic::constructor_fields_are_keyword_only(db, self);
        let pydantic_constructor_fields_are_optional = name == "__init__"
            && field_policy.is_pydantic()
            && pydantic::constructor_fields_are_optional(db, self);

        let instance_ty =
            Type::instance(db, self.apply_optional_specialization(db, specialization));

        let signature_from_fields = |mut parameters: Vec<_>, return_ty: Type<'db>| {
            for (field_name, field) in self.fields(db, specialization, field_policy) {
                let (init, mut default_ty, kw_only, alias, converter, strict) = match &field.kind {
                    FieldKind::NamedTuple { default_ty } => (
                        true,
                        *default_ty,
                        None,
                        None,
                        None,
                        pydantic::ConfigBoolean::Unspecified,
                    ),
                    FieldKind::Dataclass {
                        init,
                        default_ty,
                        kw_only,
                        alias,
                        converter,
                        ..
                    } => (
                        *init,
                        *default_ty,
                        *kw_only,
                        alias.as_ref(),
                        *converter,
                        pydantic::ConfigBoolean::Unspecified,
                    ),
                    FieldKind::Pydantic {
                        init,
                        default_ty,
                        alias,
                        strict,
                    } => (*init, *default_ty, None, alias.as_ref(), None, *strict),
                    FieldKind::TypedDict { .. } => continue,
                };
                let mut field_ty = field.declared_ty;

                if name == "__init__" && !init {
                    // Skip fields with `init=False`
                    continue;
                }

                if field.is_kw_only_sentinel(db) {
                    // Attributes annotated with `dataclass.KW_ONLY` are not present in the synthesized
                    // `__init__` method; they are used to indicate that the following parameters are
                    // keyword-only.
                    continue;
                }

                let dunder_set = field_ty.class_member(db, "__set__".into());
                if let Place::Defined(DefinedPlace {
                    ty: dunder_set,
                    definedness: Definedness::AlwaysDefined,
                    ..
                }) = dunder_set.place
                {
                    // The descriptor handling below is guarded by this not-dynamic check, because
                    // dynamic types like `Any` are valid (data) descriptors: since they have all
                    // possible attributes, they also have a (callable) `__set__` method. The
                    // problem is that we can't determine the type of the value parameter this way.
                    // Instead, we want to use the dynamic type itself in this case, so we skip the
                    // special descriptor handling.
                    if !dunder_set.is_dynamic() {
                        // This type of this attribute is a data descriptor. Instead of overwriting the
                        // descriptor attribute, data-classes will (implicitly) call the `__set__` method
                        // of the descriptor. This means that the synthesized `__init__` parameter for
                        // this attribute is determined by possible `value` parameter types with which
                        // the `__set__` method can be called.
                        //
                        // We union parameter types across overloads of a single callable, intersect
                        // callable bindings inside an intersection element, and union outer elements.
                        field_ty = dunder_set.bindings(db).map_types(db, |binding| {
                            let mut value_types = UnionBuilder::new(db);
                            let mut has_value_type = false;
                            for overload in binding {
                                if let Some(value_param) =
                                    overload.signature.parameters().get_positional(2)
                                {
                                    value_types = value_types.add(value_param.annotated_type());
                                    has_value_type = true;
                                } else if overload.signature.parameters().is_gradual() {
                                    value_types = value_types.add(Type::unknown());
                                    has_value_type = true;
                                }
                            }
                            has_value_type.then(|| value_types.build())
                        });

                        // The default value of the attribute is *not* determined by the right hand side
                        // of the class-body assignment. Instead, the runtime invokes `__get__` on the
                        // descriptor, as if it had been called on the class itself, i.e. it passes `None`
                        // for the `instance` argument.

                        if let Some(ref mut default_ty) = default_ty {
                            *default_ty = default_ty
                                .try_call_dunder_get(db, None, Type::from(self))
                                .map(|(return_ty, _)| return_ty)
                                .unwrap_or_else(Type::unknown);
                        }
                    }
                }

                if let Some((converter_input_ty, _)) = converter {
                    field_ty = converter_input_ty;
                }

                if name == "__init__"
                    && let Some(metadata) = field_policy.pydantic_metadata()
                {
                    field_ty = pydantic::constructor_parameter_type(db, field_ty, strict, metadata);
                }

                if pydantic_constructor_fields_are_optional && default_ty.is_none() {
                    default_ty = Some(Type::unknown());
                }

                let is_kw_only = matches!(name, "__replace__" | "_replace")
                    || pydantic_constructor_fields_are_keyword_only
                    || kw_only.unwrap_or(false);

                let mut add_parameter_with_name = |parameter_name, default_ty| {
                    let mut parameter = if is_kw_only {
                        Parameter::keyword_only(parameter_name)
                    } else {
                        Parameter::positional_or_keyword(parameter_name)
                    }
                    .with_annotated_type(field_ty)
                    .with_definition(field.first_declaration);

                    parameter = if matches!(name, "__replace__" | "_replace") {
                        // When replacing, we know there is a default value for the field
                        // (the value that is currently assigned to the field)
                        // assume this to be the declared type of the field
                        parameter.with_default_type(field_ty)
                    } else {
                        parameter.with_optional_default_type(default_ty)
                    };

                    parameters.push(parameter);
                };

                if name == "__init__"
                    && let Some(metadata) = field_policy.pydantic_metadata()
                    && let Some(alias) = alias
                {
                    match (
                        metadata.validates_by_alias(db),
                        metadata.validates_by_name(db),
                    ) {
                        (true, true) => {
                            let alias = Name::new(&**alias);
                            if alias == *field_name {
                                add_parameter_with_name(field_name.clone(), default_ty);
                            } else {
                                // A normal signature cannot express that at least one of two
                                // differently named parameters is required. We could solve
                                // this with overloads, but the number of overloads would grow
                                // exponentially in the number of parameters. So for now, we
                                // treat both the alias and the field name as optional
                                // parameters, which leads to false negatives if none of them
                                // is provided.
                                let default_ty = Some(default_ty.unwrap_or_else(Type::unknown));
                                add_parameter_with_name(alias, default_ty);
                                add_parameter_with_name(field_name.clone(), default_ty);
                            }
                        }
                        (true, false) => {
                            add_parameter_with_name(Name::new(&**alias), default_ty);
                        }
                        (false, true) => {
                            add_parameter_with_name(field_name.clone(), default_ty);
                        }
                        (false, false) => {}
                    }
                } else {
                    // Use the alias name if provided, otherwise use the field name.
                    let parameter_name =
                        Name::new(alias.map(|alias| &**alias).unwrap_or(&**field_name));
                    add_parameter_with_name(parameter_name, default_ty);
                }
            }

            // In the event that we have a mix of keyword-only and positional parameters, we need to sort them
            // so that the keyword-only parameters appear after positional parameters.
            parameters.sort_by_key(Parameter::is_keyword_only);

            if name == "__init__"
                && field_policy
                    .pydantic_metadata()
                    .is_some_and(|metadata| pydantic::model_init_accepts_extra(db, self, metadata))
            {
                let extra = pydantic::extra_parameter(&parameters);
                parameters.push(extra);
            }

            let signature = match name {
                "__new__" | "__init__" => Signature::new_generic(
                    inherited_generic_context.or_else(|| self.inherited_generic_context(db)),
                    Parameters::standard(parameters),
                    return_ty,
                ),
                _ => Signature::new(Parameters::standard(parameters), return_ty),
            };
            Some(Type::function_like_callable(db, signature))
        };

        match (field_policy, name) {
            (field_policy, "__init__")
                if field_policy.synthesizes_constructor_signature_from_fields() =>
            {
                if field_policy.is_dataclass_like()
                    && !self.has_dataclass_param(db, field_policy, DataclassFlags::INIT)
                {
                    return None;
                }

                let self_parameter = Parameter::positional_or_keyword(Name::new_static("self"))
                    // TODO: could be `Self`.
                    .with_annotated_type(instance_ty);
                signature_from_fields(vec![self_parameter], Type::none(db))
            }
            (
                CodeGeneratorKind::NamedTuple,
                "__new__" | "__init__" | "__match_args__" | "_replace" | "__replace__" | "_fields",
            ) if self.namedtuple_base_has_unknown_fields(db) => {
                // When the namedtuple base has unknown fields, fall back to NamedTupleFallback
                // which has generic signatures that accept any arguments.
                KnownClass::NamedTupleFallback
                    .to_class_literal(db)
                    .as_class_literal()?
                    .as_static()?
                    .own_class_member(db, inherited_generic_context, None, name)
                    .ignore_possibly_undefined()
                    .map(|ty| {
                        ty.apply_type_mapping(
                            db,
                            &TypeMapping::ReplaceSelf {
                                new_upper_bound: instance_ty,
                            },
                            TypeContext::default(),
                        )
                    })
            }
            (
                CodeGeneratorKind::NamedTuple,
                "__match_args__" | "__new__" | "_replace" | "__replace__" | "_fields" | "__slots__",
            ) => {
                let fields = self.fields(db, specialization, field_policy);
                let fields_iter = fields.iter().map(|(name, field)| {
                    let default_ty = match &field.kind {
                        FieldKind::NamedTuple { default_ty } => *default_ty,
                        _ => None,
                    };
                    NamedTupleField {
                        name: name.clone(),
                        ty: field.declared_ty,
                        default: default_ty,
                        definition: field.first_declaration,
                    }
                });
                synthesize_namedtuple_class_member(
                    db,
                    name,
                    instance_ty,
                    fields_iter,
                    specialization.map(|s| s.generic_context(db)),
                )
            }
            (
                field_policy @ CodeGeneratorKind::DataclassLike(_),
                "__lt__" | "__le__" | "__gt__" | "__ge__",
            ) => {
                if !self.has_dataclass_param(db, field_policy, DataclassFlags::ORDER) {
                    return None;
                }

                let signature = Signature::new(
                    Parameters::standard([
                        Parameter::positional_or_keyword(Name::new_static("self"))
                            // TODO: could be `Self`.
                            .with_annotated_type(instance_ty),
                        Parameter::positional_or_keyword(Name::new_static("other"))
                            // TODO: could be `Self`.
                            .with_annotated_type(instance_ty),
                    ]),
                    KnownClass::Bool.to_instance(db),
                );

                Some(Type::function_like_callable(db, signature))
            }
            (field_policy @ CodeGeneratorKind::DataclassLike(_), "__hash__") => {
                let unsafe_hash =
                    self.has_dataclass_param(db, field_policy, DataclassFlags::UNSAFE_HASH);
                let frozen = self.has_dataclass_param(db, field_policy, DataclassFlags::FROZEN);
                let eq = self.has_dataclass_param(db, field_policy, DataclassFlags::EQ);

                if unsafe_hash || (frozen && eq) {
                    let signature = Signature::new(
                        Parameters::standard([Parameter::positional_or_keyword(Name::new_static(
                            "self",
                        ))
                        .with_annotated_type(instance_ty)]),
                        KnownClass::Int.to_instance(db),
                    );

                    Some(Type::function_like_callable(db, signature))
                } else if eq && !frozen {
                    Some(Type::none(db))
                } else {
                    // No `__hash__` is generated, fall back to `object.__hash__`
                    None
                }
            }
            (field_policy @ CodeGeneratorKind::DataclassLike(_), "__match_args__")
                if Program::get(db).python_version(db) >= PythonVersion::PY310 =>
            {
                if !self.has_dataclass_param(db, field_policy, DataclassFlags::MATCH_ARGS) {
                    return None;
                }

                let kw_only_default =
                    self.has_dataclass_param(db, field_policy, DataclassFlags::KW_ONLY);

                let fields = self.fields(db, specialization, field_policy);
                let match_args = fields
                    .iter()
                    .filter(|(_, field)| {
                        if let FieldKind::Dataclass { init, kw_only, .. } = &field.kind {
                            *init && !kw_only.unwrap_or(kw_only_default)
                        } else {
                            false
                        }
                    })
                    .map(|(name, _)| Type::string_literal(db, name));
                Some(Type::heterogeneous_tuple(db, match_args))
            }
            (field_policy @ CodeGeneratorKind::DataclassLike(_), "__weakref__")
                if Program::get(db).python_version(db) >= PythonVersion::PY311 =>
            {
                if !self.has_dataclass_param(db, field_policy, DataclassFlags::WEAKREF_SLOT)
                    || !self.has_dataclass_param(db, field_policy, DataclassFlags::SLOTS)
                {
                    return None;
                }

                // This could probably be `weakref | None`, but it does not seem important enough to
                // model it precisely.
                Some(UnionType::from_two_elements(
                    db,
                    Type::any(),
                    Type::none(db),
                ))
            }
            (CodeGeneratorKind::NamedTuple, name) if name != "__init__" => {
                KnownClass::NamedTupleFallback
                    .to_class_literal(db)
                    .as_class_literal()?
                    .as_static()?
                    .own_class_member(db, self.inherited_generic_context(db), None, name)
                    .ignore_possibly_undefined()
                    .map(|ty| {
                        ty.apply_type_mapping(
                            db,
                            &TypeMapping::ReplaceSelf {
                                new_upper_bound: determine_upper_bound(
                                    db,
                                    ClassLiteral::Static(self),
                                    |base| {
                                        base.into_class()
                                            .is_some_and(|c| c.is_known(db, KnownClass::Tuple))
                                    },
                                ),
                            },
                            TypeContext::default(),
                        )
                    })
            }
            (CodeGeneratorKind::DataclassLike(_), "__replace__")
                if Program::get(db).python_version(db) >= PythonVersion::PY313 =>
            {
                let self_parameter = Parameter::positional_or_keyword(Name::new_static("self"))
                    .with_annotated_type(instance_ty);

                signature_from_fields(vec![self_parameter], instance_ty)
            }
            (
                field_policy @ (CodeGeneratorKind::DataclassLike(_)
                | CodeGeneratorKind::Pydantic(_)),
                "__setattr__",
            ) => {
                if self.is_frozen_dataclass(db) == Some(true)
                    || Self::is_frozen_pydantic_model(db, field_policy)
                {
                    let signature = Signature::new(
                        Parameters::standard([
                            Parameter::positional_or_keyword(Name::new_static("self"))
                                .with_annotated_type(instance_ty),
                            Parameter::positional_or_keyword(Name::new_static("name")),
                            Parameter::positional_or_keyword(Name::new_static("value")),
                        ]),
                        Type::Never,
                    );

                    return Some(Type::function_like_callable(db, signature));
                }
                None
            }
            (field_policy @ CodeGeneratorKind::DataclassLike(_), "__slots__")
                if Program::get(db).python_version(db) >= PythonVersion::PY310 =>
            {
                self.has_dataclass_param(db, field_policy, DataclassFlags::SLOTS)
                    .then(|| {
                        let fields = self.fields(db, specialization, field_policy);
                        let slots = fields.keys().map(|name| Type::string_literal(db, name));
                        Type::heterogeneous_tuple(db, slots)
                    })
            }
            (CodeGeneratorKind::TypedDict, name) => synthesize_typed_dict_method(
                db,
                instance_ty
                    .as_typed_dict()
                    .expect("TypedDict code generation should use a TypedDict instance"),
                name,
                || TypedDictFields::Static(self.fields(db, specialization, field_policy)),
            ),
            _ => None,
        }
    }

    /// Synthesize a `__setattr__` view for an ordinary subclass of a frozen dataclass.
    ///
    /// CPython's generated frozen-dataclass `__setattr__` rejects all writes on exact instances of
    /// the frozen dataclass, but on subclass instances it only rejects writes to that dataclass's
    /// fields before delegating to the next `__setattr__` in the MRO.
    fn own_frozen_dataclass_subclass_setattr(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
    ) -> Option<Type<'db>> {
        if CodeGeneratorKind::from_static_class(db, self).is_some() {
            return None;
        }

        let frozen_base_fields =
            self.inherited_non_slotted_frozen_dataclass_fields(db, specialization)?;

        let instance_ty =
            Type::instance(db, self.apply_optional_specialization(db, specialization));
        let setattr_signature = |name_ty, return_ty| {
            Signature::new(
                Parameters::standard([
                    Parameter::positional_or_keyword(Name::new_static("self"))
                        .with_annotated_type(instance_ty),
                    Parameter::positional_or_keyword(Name::new_static("name"))
                        .with_annotated_type(name_ty),
                    Parameter::positional_or_keyword(Name::new_static("value")),
                ]),
                return_ty,
            )
        };

        let overloads = frozen_base_fields
            .keys()
            .map(|field| setattr_signature(Type::string_literal(db, field), Type::Never))
            .chain([setattr_signature(
                KnownClass::Str.to_instance(db),
                Type::none(db),
            )]);

        Some(Type::Callable(CallableType::new(
            db,
            CallableSignature::from_overloads(overloads),
            CallableTypeKind::FunctionLike,
            CallableFunctionProvenance::None,
        )))
    }

    /// Return the inherited frozen dataclass fields whose generated `__setattr__` still controls
    /// assignments on this class.
    fn inherited_non_slotted_frozen_dataclass_fields(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
    ) -> Option<&'db FxIndexMap<Name, Field<'db>>> {
        for base in self.iter_mro(db, specialization).skip(1) {
            let (base_class, base_specialization) = base.into_class()?.static_class_literal(db)?;

            // Stop if another class in the MRO replaces the generated frozen setter:
            //
            //   @dataclass(frozen=True)
            //   class Frozen: x: int
            //
            //   class Mutable(Frozen):
            //       def __setattr__(self, name: str, value: object) -> None: ...
            //
            //   class Child(Mutable): ...
            //
            // Writes to `Child().x` dispatch to `Mutable.__setattr__`, not to the synthesized
            // `Frozen.__setattr__`.
            if class_member(db, base_class.body_scope(db), "__setattr__")
                .ignore_possibly_undefined()
                .is_some()
            {
                return None;
            }

            if base_class.is_frozen_dataclass(db) == Some(true) {
                let field_policy @ CodeGeneratorKind::DataclassLike(_) =
                    CodeGeneratorKind::from_static_class(db, base_class)?
                else {
                    return None;
                };

                if base_class.has_dataclass_param(db, field_policy, DataclassFlags::SLOTS) {
                    return None;
                }

                return Some(base_class.fields(db, base_specialization, field_policy));
            }
        }

        None
    }

    /// Member lookup for classes that inherit from `typing.TypedDict`.
    ///
    /// This is implemented as a separate method because the item definitions on a `TypedDict`-based
    /// class are *not* accessible as class members. Instead, this mostly defers to `TypedDictFallback`,
    /// unless `name` corresponds to one of the specialized synthetic members like `__getitem__`.
    pub(crate) fn typed_dict_member(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        name: &str,
        policy: MemberLookupPolicy,
    ) -> PlaceAndQualifiers<'db> {
        if let Some(member) = self.own_synthesized_member(db, specialization, None, name) {
            Place::bound(member).into()
        } else {
            let class = match specialization {
                Some(specialization) => {
                    ClassType::Generic(GenericAlias::new(db, self, specialization))
                }
                None => self.identity_specialization(db),
            };
            let Some(module) = self.typed_dict_module(db) else {
                return Place::Undefined.into();
            };
            typed_dict_class_member(db, class, module, policy, name)
        }
    }

    /// Returns a list of all annotated attributes defined in this class, or any of its superclasses.
    ///
    /// See [`StaticClassLiteral::own_fields`] for more details.
    pub(crate) fn fields(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        field_policy: CodeGeneratorKind<'db>,
    ) -> &'db FxIndexMap<Name, Field<'db>> {
        if field_policy == CodeGeneratorKind::NamedTuple {
            // NamedTuples do not allow multiple inheritance, so it is sufficient to enumerate the
            // fields of this class only.
            return self.own_fields(db, specialization, field_policy);
        }

        self.fields_inner(db, specialization, field_policy)
    }

    #[salsa::tracked(
        returns(ref),
        cycle_initial=|_, _, _, _, _| FxIndexMap::default(),
        heap_size=get_size2::GetSize::get_heap_size
    )]
    fn fields_inner(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        field_policy: CodeGeneratorKind<'db>,
    ) -> FxIndexMap<Name, Field<'db>> {
        enum FieldSource<'db> {
            Static(StaticClassLiteral<'db>, Option<Specialization<'db>>),
            DynamicTypedDict(DynamicTypedDictLiteral<'db>),
        }

        debug_assert_ne!(
            field_policy,
            CodeGeneratorKind::NamedTuple,
            "Collecting `fields` for NamedTuples should short-circuit in `fields()`"
        );

        let mut map: FxIndexMap<_, _> = self
            .iter_mro(db, specialization)
            .rev()
            .filter_map(|superclass| {
                let class = superclass.into_class()?;

                if let Some((class_literal, specialization)) = class.static_class_literal(db) {
                    // Pydantic collects annotated attributes from every class in the model's MRO,
                    // including ordinary classes that are not themselves Pydantic models.
                    if field_policy.is_pydantic() || field_policy.matches(db, class_literal.into())
                    {
                        return Some(FieldSource::Static(class_literal, specialization));
                    }
                }

                if field_policy == CodeGeneratorKind::TypedDict
                    && let ClassLiteral::DynamicTypedDict(typeddict) = class.class_literal(db)
                {
                    return Some(FieldSource::DynamicTypedDict(typeddict));
                }

                None
            })
            .flat_map(|source| match source {
                FieldSource::Static(class, specialization) => Either::Left(
                    class
                        .own_fields(db, specialization, field_policy)
                        .iter()
                        .map(|(name, field)| (name.clone(), field.clone())),
                ),
                FieldSource::DynamicTypedDict(typeddict) => {
                    Either::Right(typeddict.items(db).iter().map(|(name, td_field)| {
                        (
                            name.clone(),
                            Field {
                                declared_ty: td_field.declared_ty,
                                kind: FieldKind::TypedDict {
                                    is_required: td_field.is_required(),
                                    is_read_only: td_field.is_read_only(),
                                },
                                first_declaration: td_field.first_declaration(),
                            },
                        )
                    }))
                }
            })
            // KW_ONLY sentinels are markers, not real fields. Exclude them so
            // they cannot shadow an inherited field with the same name.
            .filter(|(_, field)| !field.is_kw_only_sentinel(db))
            // We collect into a FxOrderMap here to deduplicate attributes
            .collect();

        map.shrink_to_fit();
        map
    }

    pub(crate) fn validate_members(self, context: &InferContext<'db, '_>) {
        let db = context.db();
        let Some(field_policy) = CodeGeneratorKind::from_static_class(db, self) else {
            return;
        };
        let class_body_scope = self.body_scope(db);
        let table = place_table(db, class_body_scope);
        let use_def = use_def_map(db, class_body_scope);
        for (symbol_id, declarations) in use_def.all_end_of_scope_symbol_declarations() {
            let result = place_from_declarations(db, declarations.clone());
            let attr = result.ignore_conflicting_declarations();
            let symbol = table.symbol(symbol_id);
            let name = symbol.name();

            let Some(Type::FunctionLiteral(literal)) = attr.place.ignore_possibly_undefined()
            else {
                continue;
            };

            match name.as_str() {
                "__setattr__" | "__delattr__" => {
                    if field_policy.is_dataclass_like()
                        && self.is_frozen_dataclass(db) == Some(true)
                    {
                        if let Some(builder) = context.report_lint(
                            &INVALID_DATACLASS_OVERRIDE,
                            literal.node(db, context.file(), context.module()),
                        ) {
                            let mut diagnostic = builder.into_diagnostic(format_args!(
                                "Cannot overwrite attribute `{}` in frozen dataclass `{}`",
                                name,
                                self.name(db)
                            ));
                            diagnostic.info(name);
                        }
                    }
                }
                "__lt__" | "__le__" | "__gt__" | "__ge__" => {
                    if field_policy.is_dataclass_like()
                        && self.has_dataclass_param(db, field_policy, DataclassFlags::ORDER)
                    {
                        if let Some(builder) = context.report_lint(
                            &INVALID_DATACLASS_OVERRIDE,
                            literal.node(db, context.file(), context.module()),
                        ) {
                            let mut diagnostic = builder.into_diagnostic(format_args!(
                                "Cannot overwrite attribute `{}` in dataclass `{}` with `order=True`",
                                name,
                                self.name(db)
                            ));
                            diagnostic.info(name);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Returns a map of all annotated attributes defined in the body of this class.
    /// This extends the `__annotations__` attribute at runtime by also including default values
    /// and computed field properties.
    ///
    /// For a class body like
    /// ```py
    /// @dataclass(kw_only=True)
    /// class C:
    ///     x: int
    ///     y: str = "hello"
    ///     z: float = field(kw_only=False, default=1.0)
    /// ```
    /// we return a map `{"x": Field, "y": Field, "z": Field}` in class-body declaration order,
    /// where each `Field` contains the annotated type, default value (if any), and field
    /// properties.
    ///
    /// **Important**: The returned `Field` objects represent our full understanding of the fields,
    /// including properties inherited from class-level dataclass parameters (like `kw_only=True`)
    /// and dataclass-transform parameters (like `kw_only_default=True`). They do not represent
    /// only what is explicitly specified in each field definition.
    #[salsa::tracked(
        returns(ref),
        cycle_initial=|_, _, _, _, _| FxIndexMap::default(),
        heap_size=get_size2::GetSize::get_heap_size
    )]
    pub(crate) fn own_fields(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        field_policy: CodeGeneratorKind<'db>,
    ) -> FxIndexMap<Name, Field<'db>> {
        let class_body_scope = self.body_scope(db);
        let table = place_table(db, class_body_scope);

        let use_def = use_def_map(db, class_body_scope);

        // `own_fields(..., NamedTuple)` is called while constructing the class's MRO because the
        // field types determine the synthesized tuple base. `typed_dict_params` also queries the
        // class's MRO, so only read the `total` default when collecting `TypedDict` fields.
        let typed_dict_fields_are_required_by_default =
            if field_policy == CodeGeneratorKind::TypedDict {
                self.typed_dict_params(db)
                    .expect("TypedDictParams should be available for CodeGeneratorKind::TypedDict")
                    .contains(TypedDictParams::TOTAL)
            } else {
                false
            };
        let dataclass_kw_only_default = field_policy
            .is_dataclass_like()
            .then(|| self.has_dataclass_param(db, field_policy, DataclassFlags::KW_ONLY));
        let mut kw_only_sentinel_field_seen = false;
        let mut field_declarations = Vec::new();

        for (symbol_id, declarations) in use_def.all_end_of_scope_symbol_declarations() {
            // Here, we exclude all declarations that are not annotated assignments. We need this because
            // things like function definitions and nested classes would otherwise be considered dataclass
            // fields. The check is too broad in the sense that it also excludes (weird) constructs where
            // a symbol would have multiple declarations, one of which is an annotated assignment. If we
            // want to improve this, we could instead pass a definition-kind filter to the use-def map
            // query, or to the `symbol_from_declarations` call below. Doing so would potentially require
            // us to generate a union of `__init__` methods.
            if declarations.clone().any_reachable(db, |declaration| {
                declaration.is_defined_and(|declaration| {
                    !matches!(
                        declaration.kind(db),
                        DefinitionKind::AnnotatedAssignment(..)
                    )
                })
            }) {
                continue;
            }

            // Field contents come from the declarations live at end of scope, but field order is
            // anchored to the first reachable annotated declaration in the class body.
            let Some(first_declaration_order) = use_def
                .reachable_symbol_declarations(symbol_id)
                .first_reachable_declaration_order(db, |declaration| {
                    declaration.is_defined_and(|declaration| {
                        matches!(
                            declaration.kind(db),
                            DefinitionKind::AnnotatedAssignment(..)
                        )
                    })
                })
            else {
                continue;
            };

            let result = place_from_declarations(db, declarations.clone());
            field_declarations.push((first_declaration_order, symbol_id, result));
        }

        field_declarations
            .sort_unstable_by_key(|(first_declaration_order, _, _)| *first_declaration_order);

        let mut attributes = FxIndexMap::default();
        for (_, symbol_id, result) in field_declarations {
            let symbol = table.symbol(symbol_id);
            let first_declaration = result.first_declaration;
            let attr = result.ignore_conflicting_declarations();
            if attr.is_class_var() {
                continue;
            }

            if let Some(attr_ty) = attr.place.ignore_possibly_undefined() {
                let mut default_ty = if field_policy == CodeGeneratorKind::TypedDict {
                    None
                } else {
                    let bindings = use_def.end_of_scope_symbol_bindings(symbol_id);
                    place_from_bindings(db, bindings)
                        .place
                        .ignore_possibly_undefined()
                };

                default_ty =
                    default_ty.map(|ty| ty.apply_optional_specialization(db, specialization));

                let mut init = true;
                let mut kw_only = None;
                let mut alias = None;
                let mut converter = None;
                let mut strict = pydantic::ConfigBoolean::Unspecified;
                if field_policy.is_pydantic() {
                    let metadata =
                        pydantic::field_metadata(db, first_declaration, default_ty, specialization);
                    default_ty = metadata.default_ty;
                    init = metadata.init;
                    alias = metadata.alias;
                    strict = metadata.strict;
                } else if let Some(Type::KnownInstance(KnownInstanceType::Field(field))) =
                    default_ty
                {
                    default_ty = field.default_type(db);
                    init = field.init(db);
                    kw_only = field.kw_only(db);
                    alias.clone_from(field.alias(db));
                    converter = field.converter(db);
                }

                let kind = match field_policy {
                    CodeGeneratorKind::NamedTuple => FieldKind::NamedTuple { default_ty },
                    CodeGeneratorKind::DataclassLike(_) => FieldKind::Dataclass {
                        default_ty,
                        init_only: attr.is_init_var(),
                        init,
                        kw_only,
                        alias,
                        converter,
                    },
                    CodeGeneratorKind::Pydantic(_) => FieldKind::Pydantic {
                        default_ty,
                        // Pydantic treats underscore-prefixed annotations as private attributes,
                        // which are instance attributes but never constructor parameters.
                        init: init && !symbol.name().starts_with('_'),
                        alias,
                        strict,
                    },
                    CodeGeneratorKind::TypedDict => {
                        let is_required = if attr.is_required() {
                            // Explicit Required[T] annotation - always required
                            true
                        } else if attr.is_not_required() {
                            // Explicit NotRequired[T] annotation - never required
                            false
                        } else {
                            // No explicit qualifier - use class default (`total` parameter)
                            typed_dict_fields_are_required_by_default
                        };

                        FieldKind::TypedDict {
                            is_required,
                            is_read_only: attr.is_read_only(),
                        }
                    }
                };

                let mut field = Field {
                    declared_ty: attr_ty.apply_optional_specialization(db, specialization),
                    kind,
                    first_declaration,
                };

                // Check if this is a KW_ONLY sentinel and mark subsequent fields as keyword-only
                if field_policy.is_dataclass_like() && field.is_kw_only_sentinel(db) {
                    kw_only_sentinel_field_seen = true;
                }

                // If no explicit kw_only setting and we've seen KW_ONLY sentinel, mark as keyword-only
                if kw_only_sentinel_field_seen {
                    if let FieldKind::Dataclass {
                        kw_only: ref mut kw @ None,
                        ..
                    } = field.kind
                    {
                        *kw = Some(true);
                    }
                }

                // Resolve the kw_only to the class-level default. This ensures that when fields
                // are inherited by child classes, they use their defining class's kw_only default.
                if let FieldKind::Dataclass {
                    kw_only: ref mut kw @ None,
                    ..
                } = field.kind
                {
                    *kw = dataclass_kw_only_default;
                }

                attributes.insert(symbol.name().clone(), field);
            }
        }

        attributes.shrink_to_fit();

        attributes
    }

    /// Look up an instance attribute (available in `__dict__`) of the given name.
    ///
    /// See [`Type::instance_member`] for more details.
    pub(super) fn instance_member(
        self,
        db: &'db dyn Db,
        specialization: Option<Specialization<'db>>,
        name: &str,
    ) -> PlaceAndQualifiers<'db> {
        if self.is_typed_dict(db) {
            return Place::Undefined.into();
        }

        match MroLookup::new(db, self.iter_mro(db, specialization)).instance_member(name) {
            InstanceMemberResult::Done(result) => result,
            InstanceMemberResult::TypedDict => KnownClass::TypedDictFallback
                .to_instance(db)
                .instance_member(db, name)
                .map_type(|ty| {
                    ty.apply_type_mapping(
                        db,
                        &TypeMapping::ReplaceSelf {
                            new_upper_bound: Type::instance(db, self.unknown_specialization(db)),
                        },
                        TypeContext::default(),
                    )
                }),
        }
    }

    /// Tries to find declarations/bindings of an attribute named `name` that are only
    /// "implicitly" defined (`self.x = …`, `cls.x = …`) in a method of the class that
    /// corresponds to `class_body_scope`. The `target_method_decorator` parameter is
    /// used to skip methods that do not have the expected decorator.
    fn implicit_attribute(
        db: &'db dyn Db,
        class_body_scope: ScopeId<'db>,
        name: &str,
        target_method_decorator: MethodDecorator,
    ) -> Member<'db> {
        // Collect names in a tracked query so unrelated edits can preserve dependent member
        // lookups, and avoid retaining query entries for names that no method can define.
        if implicit_attribute_names(db, class_body_scope)
            .binary_search_by(|candidate| candidate.as_str().cmp(name))
            .is_err()
        {
            return Member::unbound();
        }

        Self::implicit_attribute_inner(
            db,
            ImplicitAttributeName::new(db, class_body_scope, name, target_method_decorator),
        )
    }

    #[salsa::tracked(
        returns(copy),
        cycle_fn=implicit_attribute_cycle_recover,
        cycle_initial=|_, id, _| Member {
            inner: Place::bound(Type::divergent(id)).into(),
        },
        heap_size=ruff_memory_usage::heap_size,
    )]
    fn implicit_attribute_inner(
        db: &'db dyn Db,
        attribute: ImplicitAttributeName<'db>,
    ) -> Member<'db> {
        let class_body_scope = attribute.class_body_scope(db);
        let name = attribute.name(db);
        let target_method_decorator = attribute.target_method_decorator(db);

        // If we do not see any declarations of an attribute, neither in the class body nor in
        // any method, we build a union of the raw types inferred from all bindings of that
        // attribute, then apply public-type promotion to the final union.
        let mut union_of_inferred_types = UnionBuilder::new(db);
        let mut qualifiers = TypeQualifiers::IMPLICIT_INSTANCE_ATTRIBUTE;

        let mut is_attribute_bound = false;
        let mut provenance = Provenance::Unknown;

        let file = class_body_scope.file(db);
        let module = parsed_module(db, file).load(db);
        let index = semantic_index(db, file);
        let class_map = use_def_map(db, class_body_scope);
        let class_table = place_table(db, class_body_scope);
        let is_valid_scope = |method_scope: &Scope| {
            let Some(method_def) = method_scope.node().as_function() else {
                return true;
            };

            // Check the decorators directly on the AST node to determine if this method
            // is a classmethod or staticmethod. This is more reliable than checking the
            // final evaluated type, which may be wrapped by other decorators like @cache.
            let function_node = method_def.node(&module);
            let definition = index.expect_single_definition(method_def);

            let mut is_classmethod = false;
            let mut is_staticmethod = false;

            for decorator in &function_node.decorator_list {
                let decorator_ty =
                    definition_expression_type(db, definition, &decorator.expression);
                if let Type::ClassLiteral(class) = decorator_ty {
                    match class.known(db) {
                        Some(KnownClass::Classmethod) => is_classmethod = true,
                        Some(KnownClass::Staticmethod) => is_staticmethod = true,
                        _ => {}
                    }
                }
            }

            // Also check for implicit classmethods/staticmethods based on method name
            let method_name = function_node.name.as_str();
            if is_implicit_classmethod(method_name) {
                is_classmethod = true;
            }
            if is_implicit_staticmethod(method_name) {
                is_staticmethod = true;
            }

            match target_method_decorator {
                MethodDecorator::None => !is_classmethod && !is_staticmethod,
                MethodDecorator::ClassMethod => is_classmethod,
                MethodDecorator::StaticMethod => is_staticmethod,
            }
        };

        // First check declarations
        for (attribute_declarations, method_scope_id) in
            attribute_declarations(db, class_body_scope, name)
        {
            let method_scope = index.scope(method_scope_id);
            if !is_valid_scope(method_scope) {
                continue;
            }

            for attribute_declaration in attribute_declarations {
                let DefinitionState::Defined(declaration) = attribute_declaration.declaration
                else {
                    continue;
                };

                let DefinitionKind::AnnotatedAssignment(assignment) = declaration.kind(db) else {
                    continue;
                };

                // We found an annotated assignment of one of the following forms (using 'self' in these
                // examples, but we support arbitrary names for the first parameters of methods):
                //
                //     self.name: <annotation>
                //     self.name: <annotation> = …

                let Some(annotation) = inferred_declaration(db, declaration).declared() else {
                    continue;
                };
                let annotation = Place::declared(annotation.inner)
                    .with_definition(declaration)
                    .with_qualifiers(
                        annotation.qualifiers | TypeQualifiers::IMPLICIT_INSTANCE_ATTRIBUTE,
                    );

                if let Some(all_qualifiers) = annotation.is_bare_final() {
                    if let Some(value) = assignment.value(&module) {
                        // If we see an annotated assignment with a bare `Final` as in
                        // `self.SOME_CONSTANT: Final = 1`, infer the type from the value
                        // on the right-hand side.

                        let inferred_ty = infer_expression_type(
                            db,
                            index.expression(value),
                            TypeContext::default(),
                        );
                        return Member {
                            inner: Place::bound(inferred_ty)
                                .with_definition(declaration)
                                .with_qualifiers(all_qualifiers),
                        };
                    }

                    // If there is no right-hand side, just record that we saw a `Final` qualifier
                    qualifiers |= all_qualifiers;
                    continue;
                }

                return Member { inner: annotation };
            }
        }

        for (attribute_assignments, attribute_binding_scope_id) in
            attribute_assignments(db, class_body_scope, name)
        {
            let binding_scope = index.scope(attribute_binding_scope_id);
            if !is_valid_scope(binding_scope) {
                continue;
            }

            let scope_for_reachability_analysis = {
                if binding_scope.node().as_function().is_some() {
                    binding_scope
                } else if binding_scope.is_eager() {
                    let mut eager_scope_parent = binding_scope;
                    while eager_scope_parent.is_eager()
                        && let Some(parent) = eager_scope_parent.parent()
                    {
                        eager_scope_parent = index.scope(parent);
                    }
                    eager_scope_parent
                } else {
                    binding_scope
                }
            };

            // The attribute assignment inherits the reachability of the method which contains it
            let is_method_reachable =
                if let Some(method_def) = scope_for_reachability_analysis.node().as_function() {
                    let method = index.expect_single_definition(method_def);
                    let method_place = class_table
                        .symbol_id(&method_def.node(&module).name)
                        .unwrap();
                    class_map
                        .reachable_symbol_bindings(method_place)
                        .find_map(|bind| {
                            (bind.binding.is_defined_and(|def| def == method))
                                .then(|| binding_reachability(db, class_map, &bind))
                        })
                        .unwrap_or(Truthiness::AlwaysFalse)
                } else {
                    Truthiness::AlwaysFalse
                };
            if is_method_reachable.is_always_false() {
                continue;
            }

            for attribute_assignment in attribute_assignments {
                if let DefinitionState::Undefined = attribute_assignment.binding {
                    continue;
                }

                let DefinitionState::Defined(binding) = attribute_assignment.binding else {
                    continue;
                };

                if !is_method_reachable.is_always_false() {
                    is_attribute_bound = true;
                }

                let inferred_ty = match binding.kind(db) {
                    DefinitionKind::AnnotatedAssignment(_) => {
                        // Annotated assignments were handled above. This branch is not
                        // unreachable (because of the `continue` above), but there is
                        // nothing to do here.
                        None
                    }
                    DefinitionKind::Assignment(assign) => match assign.unpack() {
                        Some(unpack) => {
                            // We found an unpacking assignment like:
                            //
                            //     .., self.name, .. = <value>
                            //     (.., self.name, ..) = <value>
                            //     [.., self.name, ..] = <value>

                            let unpacked = infer_unpack_types(db, unpack);
                            Some(unpacked.expression_type(assign.target(&module)))
                        }
                        None => {
                            // We found an un-annotated attribute assignment of the form:
                            //
                            //     self.name = <value>

                            Some(infer_expression_type(
                                db,
                                index.expression(assign.value(&module)),
                                TypeContext::default(),
                            ))
                        }
                    },
                    DefinitionKind::For(for_stmt) => match for_stmt.target_kind() {
                        TargetKind::Sequence(_, unpack) => {
                            // We found an unpacking assignment like:
                            //
                            //     for .., self.name, .. in <iterable>:

                            let unpacked = infer_unpack_types(db, unpack);
                            Some(unpacked.expression_type(for_stmt.target(&module)))
                        }
                        TargetKind::Single => {
                            // We found an attribute assignment like:
                            //
                            //     for self.name in <iterable>:

                            let iterable_ty = infer_expression_type(
                                db,
                                index.expression(for_stmt.iterable(&module)),
                                TypeContext::default(),
                            );
                            // TODO: Potential diagnostics resulting from the iterable are currently not reported.
                            Some(iterable_ty.iterate(db).homogeneous_element_type(db))
                        }
                    },
                    DefinitionKind::WithItem(with_item) => match with_item.target_kind() {
                        TargetKind::Sequence(_, unpack) => {
                            // We found an unpacking assignment like:
                            //
                            //     with <context_manager> as .., self.name, ..:

                            let unpacked = infer_unpack_types(db, unpack);
                            Some(unpacked.expression_type(with_item.target(&module)))
                        }
                        TargetKind::Single => {
                            // We found an attribute assignment like:
                            //
                            //     with <context_manager> as self.name:

                            let context_ty = infer_expression_type(
                                db,
                                index.expression(with_item.context_expr(&module)),
                                TypeContext::default(),
                            );
                            Some(if with_item.is_async() {
                                context_ty.aenter(db)
                            } else {
                                context_ty.enter(db)
                            })
                        }
                    },
                    DefinitionKind::Comprehension(comprehension) => {
                        match comprehension.target_kind() {
                            TargetKind::Sequence(_, unpack) => {
                                // We found an unpacking assignment like:
                                //
                                //     [... for .., self.name, .. in <iterable>]

                                let unpacked = infer_unpack_types(db, unpack);
                                Some(unpacked.expression_type(comprehension.target(&module)))
                            }
                            TargetKind::Single => {
                                // We found an attribute assignment like:
                                //
                                //     [... for self.name in <iterable>]

                                let iterable_ty = infer_expression_type(
                                    db,
                                    index.expression(comprehension.iterable(&module)),
                                    TypeContext::default(),
                                );
                                // TODO: Potential diagnostics resulting from the iterable are currently not reported.
                                Some(iterable_ty.iterate(db).homogeneous_element_type(db))
                            }
                        }
                    }
                    DefinitionKind::AugmentedAssignment(_) => {
                        // TODO:
                        None
                    }
                    DefinitionKind::NamedExpression(_) => {
                        // A named expression whose target is an attribute is syntactically prohibited
                        None
                    }
                    _ => None,
                };

                if let Some(inferred_ty) = inferred_ty {
                    provenance = provenance.or(Provenance::SingleDefinition(binding));
                    union_of_inferred_types = union_of_inferred_types.add(inferred_ty);
                }
            }
        }

        Member {
            inner: if is_attribute_bound {
                Place::bound(
                    union_of_inferred_types
                        .build()
                        .promote(db)
                        .promote_singletons(db),
                )
                .with_provenance(provenance)
                .with_qualifiers(qualifiers)
            } else {
                Place::Undefined.with_qualifiers(qualifiers)
            },
        }
    }

    /// A helper function for `instance_member` that looks up the `name` attribute only on
    /// this class, not on its superclasses.
    pub(super) fn own_instance_member(self, db: &'db dyn Db, name: &str) -> Member<'db> {
        // TODO: There are many things that are not yet implemented here:
        // - `typing.Final`
        // - Proper diagnostics

        // NamedTuple fields are modeled via synthesized descriptors on the class. Treating them
        // as instance attributes here causes inherited fields to leak through after a subclass
        // shadows the name with a normal class attribute.
        if CodeGeneratorKind::NamedTuple.matches(db, self.into())
            && self
                .own_fields(db, None, CodeGeneratorKind::NamedTuple)
                .contains_key(name)
        {
            return Member::unbound();
        }

        let body_scope = self.body_scope(db);
        let table = place_table(db, body_scope);

        if let Some(symbol_id) = table.symbol_id(name) {
            let use_def = use_def_map(db, body_scope);

            let declarations = use_def.end_of_scope_symbol_declarations(symbol_id);
            let declared_and_qualifiers =
                place_from_declarations(db, declarations).ignore_conflicting_declarations();

            match declared_and_qualifiers {
                PlaceAndQualifiers {
                    place:
                        mut declared @ Place::Defined(DefinedPlace {
                            ty: declared_ty,
                            definedness: declaredness,
                            provenance: declared_provenance,
                            ..
                        }),
                    qualifiers,
                } => {
                    // For the purpose of finding instance attributes, ignore `ClassVar`
                    // declarations:
                    if qualifiers.contains(TypeQualifiers::CLASS_VAR) {
                        declared = Place::Undefined;
                    }

                    if qualifiers.contains(TypeQualifiers::INIT_VAR) {
                        // We ignore `InitVar` declarations on the class body, unless that attribute is overwritten
                        // by an implicit assignment in a method
                        if Self::implicit_attribute(db, body_scope, name, MethodDecorator::None)
                            .is_undefined()
                        {
                            return Member::unbound();
                        }
                    }

                    // `KW_ONLY` sentinels are markers, not real instance attributes.
                    if declared_ty.is_instance_of(db, KnownClass::KwOnly)
                        && CodeGeneratorKind::from_static_class(db, self)
                            .is_some_and(CodeGeneratorKind::is_dataclass_like)
                    {
                        return Member::unbound();
                    }

                    // The attribute is declared in the class body.

                    let bindings = use_def.end_of_scope_symbol_bindings(symbol_id);
                    let inferred = place_from_bindings(db, bindings).place;
                    let has_binding = !inferred.is_undefined();

                    if has_binding {
                        // The attribute is declared and bound in the class body.

                        let implicit =
                            Self::implicit_attribute(db, body_scope, name, MethodDecorator::None);
                        if let Place::Defined(DefinedPlace {
                            ty: implicit_ty,
                            provenance: implicit_provenance,
                            ..
                        }) = implicit.inner.place
                        {
                            if declaredness == Definedness::AlwaysDefined {
                                // If a symbol is definitely declared, and we see
                                // attribute assignments in methods of the class,
                                // we trust the declared type.
                                Member {
                                    inner: declared.with_qualifiers(qualifiers),
                                }
                            } else {
                                Member {
                                    inner: Place::Defined(DefinedPlace {
                                        ty: UnionType::from_two_elements(
                                            db,
                                            declared_ty,
                                            implicit_ty,
                                        ),
                                        origin: TypeOrigin::Declared,
                                        definedness: declaredness,
                                        public_type_policy: PublicTypePolicy::Raw,
                                        provenance: implicit_provenance.or(declared_provenance),
                                    })
                                    .with_qualifiers(qualifiers),
                                }
                            }
                        } else if self.is_own_dataclass_instance_field(db, name)
                            && declared_ty
                                .class_member(db, "__get__".into())
                                .place
                                .is_undefined()
                        {
                            // For dataclass-like classes, declared fields are assigned
                            // by the synthesized `__init__`, so they are instance
                            // attributes even without an explicit `self.x = ...`
                            // assignment in a method body.
                            //
                            // However, if the declared type is a descriptor (has
                            // `__get__`), we return unbound so that the descriptor
                            // protocol in `member_lookup_with_policy` can resolve
                            // the attribute type through `__get__`.
                            Member {
                                inner: declared.with_qualifiers(qualifiers),
                            }
                        } else if self.is_own_plugin_instance_field(db, name) {
                            self.own_plugin_class_transform_instance_member(db, name)
                                .unwrap_or(Member {
                                    inner: declared.with_qualifiers(qualifiers),
                                })
                        } else {
                            // The symbol is declared and bound in the class body,
                            // but we did not find any attribute assignments in
                            // methods of the class. This means that the attribute
                            // has a class-level default value, but it would not be
                            // found in a `__dict__` lookup.

                            Member::unbound()
                        }
                    } else {
                        // The attribute is declared but not bound in the class body.
                        // We take this as a sign that this is intended to be a pure
                        // instance attribute, and we trust the declared type, unless
                        // it is possibly-undeclared. In the latter case, we also
                        // union with the inferred type from attribute assignments.

                        if declaredness == Definedness::AlwaysDefined {
                            Member {
                                inner: declared.with_qualifiers(qualifiers),
                            }
                        } else {
                            if let Place::Defined(DefinedPlace {
                                ty: implicit_ty,
                                provenance: implicit_provenance,
                                ..
                            }) = Self::implicit_attribute(
                                db,
                                body_scope,
                                name,
                                MethodDecorator::None,
                            )
                            .inner
                            .place
                            {
                                Member {
                                    inner: Place::Defined(DefinedPlace {
                                        ty: UnionType::from_two_elements(
                                            db,
                                            declared_ty,
                                            implicit_ty,
                                        ),
                                        origin: TypeOrigin::Declared,
                                        definedness: declaredness,
                                        public_type_policy: PublicTypePolicy::Raw,
                                        provenance: implicit_provenance.or(declared_provenance),
                                    })
                                    .with_qualifiers(qualifiers),
                                }
                            } else {
                                Member {
                                    inner: declared.with_qualifiers(qualifiers),
                                }
                            }
                        }
                    }
                }

                PlaceAndQualifiers {
                    place: Place::Undefined,
                    qualifiers: _,
                } => {
                    // The attribute is not *declared* in the class body. It could still be declared/bound
                    // in a method.

                    let implicit =
                        Self::implicit_attribute(db, body_scope, name, MethodDecorator::None);
                    if implicit.is_undefined() {
                        self.own_plugin_instance_member_after_miss(db, name)
                            .unwrap_or(implicit)
                    } else {
                        implicit
                    }
                }
            }
        } else {
            // This attribute is neither declared nor bound in the class body.
            // It could still be implicitly defined in a method.

            let implicit = Self::implicit_attribute(db, body_scope, name, MethodDecorator::None);
            if implicit.is_undefined() {
                self.own_plugin_instance_member_after_miss(db, name)
                    .unwrap_or(implicit)
            } else {
                implicit
            }
        }
    }

    /// Returns `true` if `name` is a non-init-only field directly declared on this
    /// dataclass (i.e., a field that corresponds to an instance attribute).
    ///
    /// This is used to decide whether a bare class-body annotation like `x: int`
    /// should be treated as defining an instance attribute: dataclass fields are
    /// implicitly assigned in `__init__`, so they behave as instance attributes
    /// even though no explicit binding exists in the class body.
    fn is_own_dataclass_instance_field(self, db: &'db dyn Db, name: &str) -> bool {
        let Some(field_policy) = CodeGeneratorKind::from_static_class(db, self) else {
            return false;
        };
        if !field_policy.treats_fields_as_instance_attributes() {
            return false;
        }

        let fields = self.own_fields(db, None, field_policy);
        let Some(field) = fields.get(name) else {
            return false;
        };
        matches!(
            field.kind,
            FieldKind::Dataclass {
                init_only: false,
                ..
            } | FieldKind::Pydantic { .. }
        )
    }

    /// Returns the converter's input type (i.e., the type of its first positional parameter) for a
    /// dataclass field, if the field has a converter function specified.
    pub(super) fn converter_input_type_for_field(
        self,
        db: &'db dyn Db,
        name: &str,
    ) -> Option<Type<'db>> {
        let field_policy @ CodeGeneratorKind::DataclassLike(_) =
            CodeGeneratorKind::from_static_class(db, self)?
        else {
            return None;
        };
        let fields = self.fields(db, None, field_policy);
        let field = fields.get(name)?;
        if let FieldKind::Dataclass { converter, .. } = field.kind {
            converter.map(|(input_ty, _)| input_ty)
        } else {
            None
        }
    }

    pub(super) fn to_non_generic_instance(self, db: &'db dyn Db) -> Type<'db> {
        Type::instance(db, ClassType::NonGeneric(self.into()))
    }

    /// Return this class' involvement in an inheritance cycle, if any.
    ///
    /// A class definition like this will fail at runtime,
    /// but we must be resilient to it or we could panic.
    pub(crate) fn inheritance_cycle(self, db: &'db dyn Db) -> Option<InheritanceCycle> {
        #[salsa::tracked(returns(copy), cycle_initial=|_, _, _| None, heap_size=ruff_memory_usage::heap_size)]
        fn inheritance_cycle_inner<'db>(
            db: &'db dyn Db,
            class: StaticClassLiteral<'db>,
        ) -> Option<InheritanceCycle> {
            /// Return `true` if the class is cyclically defined.
            ///
            /// Also, populates `visited_classes` with all base classes of `class`.
            fn is_cyclically_defined_recursive<'db>(
                db: &'db dyn Db,
                class: StaticClassLiteral<'db>,
                classes_on_stack: &mut FxIndexSet<StaticClassLiteral<'db>>,
                visited_classes: &mut FxIndexSet<StaticClassLiteral<'db>>,
            ) -> bool {
                let mut result = false;
                for explicit_base in class.explicit_bases(db) {
                    let explicit_base_class_literal = match explicit_base {
                        Type::ClassLiteral(class_literal) => class_literal.as_static(),
                        Type::GenericAlias(generic_alias) => Some(generic_alias.origin(db)),
                        _ => continue,
                    };
                    let Some(explicit_base_class_literal) = explicit_base_class_literal else {
                        continue;
                    };
                    if !classes_on_stack.insert(explicit_base_class_literal) {
                        return true;
                    }

                    if visited_classes.insert(explicit_base_class_literal) {
                        // If we find a cycle, keep searching to check if we can reach the starting
                        // class.
                        result |= is_cyclically_defined_recursive(
                            db,
                            explicit_base_class_literal,
                            classes_on_stack,
                            visited_classes,
                        );
                    }
                    classes_on_stack.pop();
                }
                result
            }

            tracing::trace!("Class::inheritance_cycle: {}", class.name(db));

            let visited_classes = &mut FxIndexSet::default();
            if !is_cyclically_defined_recursive(
                db,
                class,
                &mut FxIndexSet::default(),
                visited_classes,
            ) {
                None
            } else if visited_classes.contains(&class) {
                Some(InheritanceCycle::Participant)
            } else {
                Some(InheritanceCycle::Inherited)
            }
        }

        if !self.has_explicit_bases(db) {
            return None;
        }
        inheritance_cycle_inner(db, self)
    }

    /// Returns a [`Span`] with the range of the class's header.
    ///
    /// See [`Self::header_range`] for more details.
    pub(crate) fn header_span(self, db: &'db dyn Db) -> Span {
        Span::from(self.file(db)).with_range(self.header_range(db))
    }

    /// Returns the range of the class's "header": the class name
    /// and any arguments passed to the `class` statement. E.g.
    ///
    /// ```ignore
    /// class Foo(Bar, metaclass=Baz): ...
    ///       ^^^^^^^^^^^^^^^^^^^^^^^
    /// ```
    pub(crate) fn header_range(self, db: &'db dyn Db) -> TextRange {
        let class_scope = self.body_scope(db);
        let module = parsed_module(db, class_scope.file(db)).load(db);
        let class_node = self.node(db, &module);
        let class_name = &class_node.name;
        TextRange::new(
            class_name.start(),
            class_node
                .arguments
                .as_deref()
                .map(Ranged::end)
                .unwrap_or_else(|| class_name.end()),
        )
    }

    /// Returns the range of the class's name
    pub(crate) fn focus_range(self, db: &'db dyn Db) -> TextRange {
        let class_scope = self.body_scope(db);
        let module = parsed_module(db, class_scope.file(db)).load(db);
        let class_node = self.node(db, &module);
        class_node.name.range()
    }
}

/// A single semantic class-base entry after expanding starred tuple bases.
#[derive(Clone, Copy)]
pub(crate) struct ExpandedClassBaseEntry<'a, 'db> {
    source_node: &'a ast::Expr,
    ty: Type<'db>,
}

impl<'a, 'db> ExpandedClassBaseEntry<'a, 'db> {
    /// Returns the source expression for this base entry.
    pub(crate) const fn source_node(self) -> &'a ast::Expr {
        self.source_node
    }

    /// Returns the semantic type of this base entry.
    pub(crate) const fn ty(self) -> Type<'db> {
        self.ty
    }
}

/// Expands a class's bases into the semantic entries used by [`StaticClassLiteral::explicit_bases`].
pub(crate) fn expanded_class_base_entries<'a, 'db>(
    db: &'db dyn Db,
    known_class: Option<KnownClass>,
    class_stmt: &'a ast::StmtClassDef,
    class_definition: Definition<'db>,
) -> Vec<ExpandedClassBaseEntry<'a, 'db>> {
    match known_class {
        // Special-case `NotImplementedType`: typeshed says that it inherits from `Any`,
        // but this causes more problems than it fixes.
        Some(KnownClass::NotImplementedType) => vec![],
        _ => {
            let mut expanded_bases = Vec::with_capacity(class_stmt.bases().len());

            for base_node in class_stmt.bases() {
                if let Some(tuple) =
                    expanded_fixed_length_starred_class_base_tuple(db, class_definition, base_node)
                {
                    if let ast::Expr::Starred(starred) = base_node
                        && let Some(tuple_literal) = starred.value.as_tuple_expr()
                        && tuple_literal.len() == tuple.len()
                        && tuple_literal
                            .iter()
                            .all(|element| !element.is_starred_expr())
                    {
                        expanded_bases.extend(
                            tuple_literal
                                .iter()
                                .zip(tuple.owned_elements().into_vec())
                                .map(|(source_node, ty)| ExpandedClassBaseEntry {
                                    source_node,
                                    ty,
                                }),
                        );
                        continue;
                    }

                    expanded_bases.extend(tuple.owned_elements().into_vec().into_iter().map(
                        |ty| ExpandedClassBaseEntry {
                            source_node: base_node,
                            ty,
                        },
                    ));
                    continue;
                }

                let ty = if matches!(base_node, ast::Expr::Starred(_)) {
                    Type::unknown()
                } else {
                    definition_expression_type(db, class_definition, base_node)
                };
                expanded_bases.push(ExpandedClassBaseEntry {
                    source_node: base_node,
                    ty,
                });
            }

            expanded_bases
        }
    }
}

/// If `base_node` is a starred class base whose value is inferred as a fixed-length tuple,
/// returns the unpacked tuple in source order.
fn expanded_fixed_length_starred_class_base_tuple<'db>(
    db: &'db dyn Db,
    class_definition: Definition<'db>,
    base_node: &ast::Expr,
) -> Option<FixedLengthTuple<Type<'db>>> {
    let ast::Expr::Starred(starred) = base_node else {
        return None;
    };

    let starred_ty = definition_expression_type(db, class_definition, &starred.value);
    let tuple_spec = starred_ty.tuple_instance_spec(db)?;
    let Tuple::Fixed(tuple) = tuple_spec.into_owned() else {
        return None;
    };
    Some(tuple)
}

fn plugin_class_transform_route_candidates<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> Vec<String> {
    let mut candidates = vec![ClassLiteral::Static(class).qualified_name(db).to_string()];

    for base in class.iter_mro(db, None) {
        let ClassBase::Class(base_class) = base else {
            continue;
        };
        candidates.push(base_class.qualified_name(db).to_string());
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

#[salsa::tracked(
    returns(ref),
    cycle_initial=|_, _, _| PluginProjectIndex::default(),
    heap_size=get_size2::GetSize::get_heap_size
)]
fn plugin_project_index<'db>(
    db: &'db dyn Db,
    plugin_id: SemanticPluginId<'db>,
) -> PluginProjectIndex<'db> {
    let semantic_plugins = Program::get(db).semantic_plugins(db);
    let Some(plugin) = semantic_plugins
        .plugins()
        .iter()
        .find(|plugin| plugin.id() == plugin_id.id(db) && plugin.project_index_enabled())
    else {
        return PluginProjectIndex::default();
    };

    let settings = plugin_settings_summaries(db, plugin);
    let settings_diagnostics = settings
        .iter()
        .flat_map(|settings| settings.diagnostics.iter().cloned())
        .filter_map(plugin_project_diagnostic_from_protocol)
        .collect::<Vec<_>>();

    let mut config: serde_json::Value =
        serde_json::from_str(plugin.config_json()).unwrap_or_default();
    if let serde_json::Value::Object(config) = &mut config {
        config
            .entry("strict_settings")
            .or_insert_with(|| serde_json::Value::Bool(plugin.strict_settings()));
    }

    let request = protocol::PluginRequest::BuildProjectIndex(protocol::BuildProjectIndexRequest {
        context: protocol::ProjectContext {
            root: String::new(),
            python_version: Program::get(db).python_version(db).to_string(),
            platform: Program::get(db).python_platform(db).to_string(),
            config,
        },
        classes: plugin_project_class_summaries(db),
        settings,
        assignments: plugin_project_assignment_summaries(db),
        previous_index_fingerprint: None,
    });
    tracing::trace!(
        plugin_id = plugin.id(),
        ?request,
        "executing project-index plugin"
    );

    let response = execute_project_index_plugin(db, plugin, &request);
    tracing::trace!(
        plugin_id = plugin.id(),
        ?response,
        "received project-index response"
    );
    let protocol::PluginResponse::ProjectIndex(response) = response else {
        return PluginProjectIndex::default();
    };

    let virtual_types = plugin_virtual_type_patches_from_protocol(db, response.virtual_types);
    let mut contributions = response
        .contributions
        .into_iter()
        .filter_map(|contribution| {
            plugin_contribution_to_patch(db, contribution, virtual_types.as_ref())
        })
        .collect::<Vec<_>>();
    contributions.sort_by(|left, right| {
        plugin_contribution_sort_key(left).cmp(&plugin_contribution_sort_key(right))
    });
    contributions.dedup_by(|left, right| {
        left.target == right.target && left.patch.sort_name() == right.patch.sort_name()
    });

    PluginProjectIndex {
        plugin_index_json: (!response.plugin_index.is_null())
            .then(|| serde_json::to_string(&response.plugin_index).ok())
            .flatten(),
        contributions: contributions.into_boxed_slice(),
        virtual_types,
        diagnostics: settings_diagnostics
            .into_iter()
            .chain(
                response
                    .diagnostics
                    .into_iter()
                    .filter_map(plugin_project_diagnostic_from_protocol),
            )
            .collect(),
    }
}

pub(crate) fn plugin_project_index_diagnostics_for_file(
    db: &dyn Db,
    file: File,
) -> Vec<Diagnostic> {
    let semantic_plugins = Program::get(db).semantic_plugins(db);
    if semantic_plugins.is_empty() {
        return Vec::new();
    }

    semantic_plugins
        .plugins()
        .iter()
        .filter(|plugin| plugin.project_index_enabled())
        .flat_map(|plugin| {
            plugin_project_index(db, SemanticPluginId::new(db, plugin.id().to_string()))
                .diagnostics
                .iter()
                .filter_map(|diagnostic| {
                    plugin_project_diagnostic_to_diagnostic(db, file, diagnostic)
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn plugin_project_diagnostic_from_protocol(
    diagnostic: protocol::PluginDiagnostic,
) -> Option<PluginProjectDiagnostic> {
    Some(PluginProjectDiagnostic {
        id: diagnostic.id,
        message: diagnostic.message,
        severity: match diagnostic.severity {
            protocol::DiagnosticSeverity::Error => PluginProjectDiagnosticSeverity::Error,
            protocol::DiagnosticSeverity::Warning => PluginProjectDiagnosticSeverity::Warning,
            protocol::DiagnosticSeverity::Info => PluginProjectDiagnosticSeverity::Info,
        },
        location: diagnostic
            .location
            .map(|location| PluginProjectDiagnosticLocation {
                file_path: location.file_path,
                start_line: location.start.line,
                start_column: location.start.column,
                end_line: location.end.line,
                end_column: location.end.column,
            }),
    })
}

fn plugin_project_diagnostic_to_diagnostic(
    db: &dyn Db,
    file: File,
    plugin_diagnostic: &PluginProjectDiagnostic,
) -> Option<Diagnostic> {
    let location = plugin_diagnostic.location.as_ref()?;
    if location.file_path != file.path(db).to_string() {
        return None;
    }

    let source = source_text(db, file);
    let index = line_index(db, file);
    let start = index.offset(
        SourceLocation {
            line: OneIndexed::new(location.start_line as usize)?,
            character_offset: OneIndexed::new(location.start_column as usize)?,
        },
        source.as_str(),
        PositionEncoding::Utf32,
    );
    let end = index.offset(
        SourceLocation {
            line: OneIndexed::new(location.end_line as usize)?,
            character_offset: OneIndexed::new(location.end_column as usize)?,
        },
        source.as_str(),
        PositionEncoding::Utf32,
    );
    let range = if start <= end {
        TextRange::new(start, end)
    } else {
        TextRange::empty(start)
    };

    let mut diagnostic = Diagnostic::new(
        DiagnosticId::PluginConfiguration,
        match plugin_diagnostic.severity {
            PluginProjectDiagnosticSeverity::Error => Severity::Error,
            PluginProjectDiagnosticSeverity::Warning => Severity::Warning,
            PluginProjectDiagnosticSeverity::Info => Severity::Info,
        },
        plugin_diagnostic.message.as_str(),
    );
    diagnostic.annotate(
        Annotation::primary(Span::from(file).with_range(range))
            .message(plugin_diagnostic.id.as_str()),
    );
    Some(diagnostic)
}

pub(crate) fn plugin_project_index_json(
    db: &dyn Db,
    plugin: &SemanticPlugin,
) -> Option<serde_json::Value> {
    if !plugin.project_index_enabled() {
        return None;
    }

    let plugin_index_json =
        plugin_project_index(db, SemanticPluginId::new(db, plugin.id().to_string()))
            .plugin_index_json
            .as_ref()?;
    serde_json::from_str(plugin_index_json).ok()
}

pub(crate) fn plugin_project_index_virtual_types<'db>(
    db: &'db dyn Db,
    plugin: &SemanticPlugin,
) -> &'db [PluginVirtualTypePatch<'db>] {
    if !plugin.project_index_enabled() {
        return &[];
    }

    &plugin_project_index(db, SemanticPluginId::new(db, plugin.id().to_string())).virtual_types
}

fn plugin_project_class_summaries(db: &dyn Db) -> Vec<protocol::ClassSummary> {
    let mut summaries = Vec::new();

    for module in all_modules(db) {
        let Some(file) = module.file(db) else {
            continue;
        };
        if !db.should_check_file(file) {
            continue;
        }

        let parsed = parsed_module(db, file).load(db);
        for statement in &parsed.syntax().body {
            let Some(class) = static_class_literal_from_statement(db, file, statement) else {
                continue;
            };
            let summary = plugin_class_summary(db, class);
            summaries.push(plugin_protocol_class_summary(db, class, &summary));
        }
    }

    summaries
}

fn plugin_project_assignment_summaries(db: &dyn Db) -> Vec<protocol::AssignmentSummary> {
    let mut summaries = Vec::new();

    for module in all_modules(db) {
        let Some(file) = module.file(db) else {
            continue;
        };
        if !db.should_check_file(file) {
            continue;
        }

        let parsed = parsed_module(db, file).load(db);
        let index = semantic_index(db, file);
        let module_name = plugin_module_name(db, file);
        for statement in &parsed.syntax().body {
            let (target, value, definition) = match statement {
                ast::Stmt::Assign(assign) if assign.targets.len() == 1 => {
                    let Some(target) = assign.targets[0].as_name_expr() else {
                        continue;
                    };
                    let Some(definition) = index.try_definition(target) else {
                        continue;
                    };
                    (target, assign.value.as_ref(), definition)
                }
                ast::Stmt::AnnAssign(assign) => {
                    let Some(target) = assign.target.as_name_expr() else {
                        continue;
                    };
                    let Some(value) = assign.value.as_deref() else {
                        continue;
                    };
                    let Some(definition) = index.try_definition(assign) else {
                        continue;
                    };
                    (target, value, definition)
                }
                _ => continue,
            };
            let inferred_ty = definition_expression_type(db, definition, value);
            let name = target.id.to_string();
            summaries.push(protocol::AssignmentSummary {
                name: name.clone(),
                qualified_name: format!("{module_name}.{name}"),
                assigned_value: plugin_assigned_value_summary(
                    db,
                    definition,
                    value,
                    Some(inferred_ty),
                ),
                inferred_type: Some(plugin_type_expr_from_type(db, inferred_ty)),
                source: plugin_symbol_source(
                    db,
                    file,
                    target.range(),
                    Some(format!("{module_name}.{name}")),
                ),
            });
        }
    }

    summaries
}

fn plugin_settings_summaries(
    db: &dyn Db,
    plugin: &SemanticPlugin,
) -> Vec<protocol::SettingsModuleSummary> {
    plugin
        .settings_module_claims()
        .iter()
        .map(|module_name| {
            plugin_settings_module_summary(db, module_name, plugin.strict_settings())
        })
        .collect()
}

fn plugin_settings_module_summary(
    db: &dyn Db,
    module_name: &str,
    strict: bool,
) -> protocol::SettingsModuleSummary {
    let Some(file) = plugin_settings_module_file(db, module_name) else {
        return protocol::SettingsModuleSummary {
            module: module_name.to_string(),
            values: Vec::new(),
            dependencies: Vec::new(),
            diagnostics: vec![plugin_settings_diagnostic(
                "ty.settings.module-not-found",
                format!("Settings module `{module_name}` could not be resolved"),
                None,
                plugin_settings_diagnostic_severity(strict),
            )],
            source: protocol::SymbolSource {
                module: Some(module_name.to_string()),
                ..protocol::SymbolSource::default()
            },
        };
    };

    let parsed = parsed_module(db, file).load(db);
    let mut values = Vec::new();
    let mut diagnostics = Vec::new();
    let mut dependencies = BTreeSet::from([file.path(db).to_string()]);
    let settings_bindings = parsed
        .syntax()
        .body
        .iter()
        .filter_map(setting_assignment_from_statement)
        .filter(|(name, _, _)| is_static_settings_name(name))
        .map(|(name, value, _)| (name, value))
        .collect::<BTreeMap<_, _>>();
    let import_bindings = plugin_settings_import_bindings(parsed.syntax().body.as_slice());

    for statement in &parsed.syntax().body {
        if let Some(value) = setting_summary_from_statement(
            db,
            file,
            module_name,
            statement,
            &settings_bindings,
            &import_bindings,
            &mut dependencies,
            &mut diagnostics,
            strict,
        ) {
            values.push(value);
        }
    }

    protocol::SettingsModuleSummary {
        module: module_name.to_string(),
        values,
        dependencies: dependencies
            .into_iter()
            .map(|path| protocol::PluginDependency { path, sha256: None })
            .collect(),
        diagnostics,
        source: protocol::SymbolSource {
            module: Some(module_name.to_string()),
            file_path: Some(file.path(db).to_string()),
            ..protocol::SymbolSource::default()
        },
    }
}

fn plugin_settings_module_file(db: &dyn Db, module_name: &str) -> Option<File> {
    for module in all_modules(db) {
        if module.name(db).to_string() == module_name {
            return module.file(db);
        }
    }
    None
}

#[derive(Default)]
struct PluginSettingsImportBindings {
    values: BTreeMap<String, (String, String)>,
    modules: BTreeMap<String, String>,
}

fn plugin_settings_import_bindings(statements: &[ast::Stmt]) -> PluginSettingsImportBindings {
    let mut bindings = PluginSettingsImportBindings::default();

    for statement in statements {
        match statement {
            ast::Stmt::Import(import) => {
                for alias in &import.names {
                    let imported_module = alias.name.as_str();
                    let local_name = alias.asname.as_ref().map_or_else(
                        || imported_module.split('.').next(),
                        |asname| Some(asname.as_str()),
                    );
                    if let Some(local_name) = local_name {
                        bindings
                            .modules
                            .insert(local_name.to_string(), imported_module.to_string());
                    }
                }
            }
            ast::Stmt::ImportFrom(import_from) if import_from.level == 0 => {
                let Some(module) = import_from.module.as_ref() else {
                    continue;
                };
                let module = module.as_str();
                for alias in &import_from.names {
                    let imported_name = alias.name.as_str();
                    if !is_static_settings_name(imported_name) {
                        continue;
                    }
                    let local_name = alias
                        .asname
                        .as_ref()
                        .map_or(imported_name, |asname| asname.as_str());
                    bindings.values.insert(
                        local_name.to_string(),
                        (module.to_string(), imported_name.to_string()),
                    );
                }
            }
            _ => {}
        }
    }

    bindings
}

fn setting_assignment_from_statement(
    statement: &ast::Stmt,
) -> Option<(String, &ast::Expr, TextRange)> {
    match statement {
        ast::Stmt::Assign(assign) => {
            let target = assign.targets.first()?.as_name_expr()?;
            Some((target.id.to_string(), assign.value.as_ref(), target.range()))
        }
        ast::Stmt::AnnAssign(assign) => {
            let target = assign.target.as_name_expr()?;
            let value = assign.value.as_deref()?;
            Some((target.id.to_string(), value, target.range()))
        }
        _ => None,
    }
}

fn setting_summary_from_statement(
    db: &dyn Db,
    file: File,
    module_name: &str,
    statement: &ast::Stmt,
    settings_bindings: &BTreeMap<String, &ast::Expr>,
    import_bindings: &PluginSettingsImportBindings,
    dependencies: &mut BTreeSet<String>,
    diagnostics: &mut Vec<protocol::PluginDiagnostic>,
    strict: bool,
) -> Option<protocol::SettingValueSummary> {
    let (name, value, target_range) = setting_assignment_from_statement(statement)?;

    if !is_static_settings_name(&name) {
        return None;
    }

    let source = plugin_symbol_source(
        db,
        file,
        target_range,
        Some(format!("{module_name}.{name}")),
    );
    let value = plugin_settings_literal_value_from_expr(
        db,
        module_name,
        value,
        settings_bindings,
        import_bindings,
        &mut BTreeSet::new(),
        dependencies,
    );
    if value.is_unknown() {
        diagnostics.push(plugin_settings_diagnostic(
            "ty.settings.unsupported-value",
            format!("Setting `{module_name}.{name}` is not a supported static literal"),
            Some(&source),
            plugin_settings_diagnostic_severity(strict),
        ));
        return None;
    }

    Some(protocol::SettingValueSummary {
        name,
        value,
        source,
    })
}

fn is_static_settings_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|character| character.is_ascii_uppercase())
        && name.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '_'
        })
}

fn plugin_settings_diagnostic(
    id: impl Into<String>,
    message: impl Into<String>,
    source: Option<&protocol::SymbolSource>,
    severity: protocol::DiagnosticSeverity,
) -> protocol::PluginDiagnostic {
    protocol::PluginDiagnostic {
        id: id.into(),
        message: message.into(),
        severity,
        location: source.and_then(plugin_diagnostic_location_from_source),
        metadata: Default::default(),
    }
}

fn plugin_settings_diagnostic_severity(strict: bool) -> protocol::DiagnosticSeverity {
    if strict {
        protocol::DiagnosticSeverity::Error
    } else {
        protocol::DiagnosticSeverity::Warning
    }
}

fn plugin_diagnostic_location_from_source(
    source: &protocol::SymbolSource,
) -> Option<protocol::DiagnosticLocation> {
    Some(protocol::DiagnosticLocation {
        file_path: source.file_path.clone()?,
        start: source.start?,
        end: source.end?,
    })
}

fn static_class_literal_from_statement<'db>(
    db: &'db dyn Db,
    file: File,
    statement: &ast::Stmt,
) -> Option<StaticClassLiteral<'db>> {
    let class_node = statement.as_class_def_stmt()?;
    let definition = semantic_index(db, file).expect_single_definition(class_node);
    let ClassLiteral::Static(class) = crate::types::infer::original_class_type(db, definition)?
    else {
        return None;
    };
    Some(class)
}

fn plugin_contribution_to_patch<'db>(
    db: &'db dyn Db,
    contribution: protocol::Contribution,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> Option<PluginContributionPatch<'db>> {
    let target = match contribution.target {
        protocol::ContributionTarget::Class { qualified_name } => {
            PluginContributionTarget::Class(qualified_name)
        }
        protocol::ContributionTarget::Instance { qualified_name } => {
            PluginContributionTarget::Instance(qualified_name)
        }
        protocol::ContributionTarget::Constructor { qualified_name } => {
            PluginContributionTarget::Constructor(qualified_name)
        }
    };
    let patch = match contribution.patch {
        protocol::ContributionPatch::Member(member) => {
            PluginContributionMemberPatch::Member(PluginMemberPatch {
                name: Name::new(&member.name),
                replace_existing: member.mode == protocol::MemberPatchMode::ReplaceExisting,
                ty: plugin_member_access_to_type_with_virtual_types(
                    db,
                    &member.access,
                    virtual_types,
                ),
                read_only: member.read_only,
            })
        }
        protocol::ContributionPatch::Field(field) => PluginContributionMemberPatch::Field(
            plugin_contribution_field_to_patch(db, field, virtual_types),
        ),
        protocol::ContributionPatch::Constructor(signature) => {
            PluginContributionMemberPatch::Constructor(PluginConstructorPatch {
                parameters: signature
                    .parameters
                    .into_iter()
                    .map(|parameter| {
                        plugin_constructor_parameter_from_protocol_type_expr(
                            db,
                            parameter,
                            virtual_types,
                        )
                    })
                    .collect(),
            })
        }
        protocol::ContributionPatch::Diagnostic(_) => return None,
    };

    Some(PluginContributionPatch { target, patch })
}

fn plugin_contribution_field_to_patch<'db>(
    db: &'db dyn Db,
    field: protocol::FieldPatch,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> PluginContributionFieldPatch<'db> {
    let descriptor_class_ty = match field.descriptor.as_ref() {
        Some(protocol::MemberAccessPatch::Value { type_expr }) => Some(
            plugin_type_expr_to_type_with_virtual_types(db, type_expr, virtual_types),
        ),
        Some(protocol::MemberAccessPatch::Descriptor { class_type, .. }) => {
            class_type.as_ref().map(|type_expr| {
                plugin_type_expr_to_type_with_virtual_types(db, type_expr, virtual_types)
            })
        }
        Some(protocol::MemberAccessPatch::Callable { fallback_type, .. }) => Some(
            plugin_type_expr_to_type_with_virtual_types(db, fallback_type, virtual_types),
        ),
        None => None,
    };

    PluginContributionFieldPatch {
        name: Name::new(&field.name),
        replace_existing: field.mode == protocol::MemberPatchMode::ReplaceExisting,
        descriptor_class_ty,
        instance_get_ty: plugin_type_expr_to_type_with_virtual_types(
            db,
            &field.instance_get_type,
            virtual_types,
        ),
        instance_set_ty: field.instance_set_type.as_ref().map(|type_expr| {
            plugin_type_expr_to_type_with_virtual_types(db, type_expr, virtual_types)
        }),
    }
}

fn plugin_contribution_sort_key(
    contribution: &PluginContributionPatch<'_>,
) -> (String, u8, String) {
    let (qualified_name, scope_order) = match &contribution.target {
        PluginContributionTarget::Class(qualified_name) => (qualified_name.as_str(), 0),
        PluginContributionTarget::Instance(qualified_name) => (qualified_name.as_str(), 1),
        PluginContributionTarget::Constructor(qualified_name) => (qualified_name.as_str(), 2),
    };
    (
        qualified_name.to_string(),
        scope_order,
        contribution.patch.sort_name().to_string(),
    )
}

fn plugin_class_summary<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
) -> PluginClassSummary<'db> {
    let file = class.file(db);
    let module = parsed_module(db, file).load(db);
    let class_node = class.node(db, &module);
    let class_definition = semantic_index(db, file).expect_single_definition(class_node);
    let qualified_name = ClassLiteral::Static(class).qualified_name(db).to_string();

    let fields = plugin_class_field_summaries(db, class, &module);
    let decorators = class_node
        .decorator_list
        .iter()
        .map(|decorator| plugin_call_or_symbol_summary(db, class_definition, &decorator.expression))
        .collect();
    let metaclass = class_node.arguments.as_ref().and_then(|arguments| {
        arguments
            .keywords
            .iter()
            .find(|keyword| keyword.arg.as_ref().is_some_and(|arg| arg == "metaclass"))
            .map(|keyword| definition_expression_type(db, class_definition, &keyword.value))
    });
    let nested_classes = class_node
        .body
        .iter()
        .filter_map(|statement| nested_class_summary(db, file, &module, &qualified_name, statement))
        .collect();
    let methods = class_node
        .body
        .iter()
        .filter_map(|statement| {
            plugin_method_summary(db, file, &module, statement, &qualified_name)
        })
        .collect();
    let class_constants = class_node
        .body
        .iter()
        .filter_map(|statement| constant_summary_from_statement(db, file, &module, statement, None))
        .collect();

    PluginClassSummary {
        fields,
        methods,
        decorators,
        metaclass,
        nested_classes,
        class_constants,
        source: plugin_symbol_source(db, file, class_node.range(), Some(qualified_name)),
    }
}

fn plugin_class_field_summaries<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    module: &ParsedModuleRef,
) -> Vec<PluginClassFieldSummary<'db>> {
    let class_body_scope = class.body_scope(db);
    let file = class.file(db);
    let class_definition = class.definition(db);
    let table = place_table(db, class_body_scope);
    let use_def = use_def_map(db, class_body_scope);
    let mut field_definitions = Vec::new();

    for (symbol_id, declarations) in use_def.all_end_of_scope_symbol_declarations() {
        let declaration_result = place_from_declarations(db, declarations.clone());
        let attr = declaration_result.ignore_conflicting_declarations();
        if attr.is_class_var() || attr.is_init_var() {
            continue;
        }

        let annotated_definition = use_def
            .reachable_symbol_declarations(symbol_id)
            .filter_map(|declaration| {
                let DefinitionState::Defined(definition) = declaration.declaration else {
                    return None;
                };
                matches!(definition.kind(db), DefinitionKind::AnnotatedAssignment(..))
                    .then_some((declaration.declaration_order, definition))
            })
            .min_by_key(|(order, _)| *order);

        let binding_definition = use_def
            .end_of_scope_symbol_bindings(symbol_id)
            .filter_map(|binding| {
                let DefinitionState::Defined(definition) = binding.binding else {
                    return None;
                };
                matches!(
                    definition.kind(db),
                    DefinitionKind::Assignment(..) | DefinitionKind::AnnotatedAssignment(..)
                )
                .then_some((binding.binding_order, definition))
            })
            .min_by_key(|(order, _)| *order);

        let Some((first_definition_order, definition)) =
            annotated_definition.or(binding_definition)
        else {
            continue;
        };

        field_definitions.push((
            first_definition_order,
            symbol_id,
            definition,
            attr.place.ignore_possibly_undefined(),
        ));
    }

    for (symbol_id, bindings) in use_def.all_end_of_scope_symbol_bindings() {
        if field_definitions
            .iter()
            .any(|(_, existing_symbol_id, _, _)| *existing_symbol_id == symbol_id)
        {
            continue;
        }

        let Some((first_definition_order, definition)) = bindings
            .filter_map(|binding| {
                let DefinitionState::Defined(definition) = binding.binding else {
                    return None;
                };
                matches!(definition.kind(db), DefinitionKind::Assignment(..))
                    .then_some((binding.binding_order, definition))
            })
            .min_by_key(|(order, _)| *order)
        else {
            continue;
        };

        field_definitions.push((first_definition_order, symbol_id, definition, None));
    }

    field_definitions.sort_unstable_by_key(|(definition_order, _, _, _)| *definition_order);

    let mut fields = Vec::new();
    for (_, symbol_id, definition, annotation_ty) in field_definitions {
        let symbol = table.symbol(symbol_id);
        let binding_place =
            place_from_bindings(db, use_def.end_of_scope_symbol_bindings(symbol_id))
                .place
                .ignore_possibly_undefined();
        let value = definition.kind(db).value(module);

        fields.push(PluginClassFieldSummary {
            name: symbol.name().clone(),
            annotation: annotation_ty,
            assigned_value: value.map(|value| {
                plugin_assigned_value_summary(db, class_definition, value, binding_place)
            }),
            inferred_type: binding_place.or(annotation_ty),
            has_default: value.is_some(),
            source: plugin_symbol_source(db, file, definition.kind(db).target_range(module), None),
        });
    }

    fields
}

fn plugin_analyze_class_request<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    summary: &PluginClassSummary<'db>,
    project_index: Option<serde_json::Value>,
) -> protocol::PluginRequest {
    let file = class.file(db);

    protocol::PluginRequest::AnalyzeClass(protocol::AnalyzeClassRequest {
        context: plugin_semantic_context(db, file, false),
        class: plugin_protocol_class_summary(db, class, summary),
        project_index,
    })
}

fn plugin_protocol_class_summary<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    summary: &PluginClassSummary<'db>,
) -> protocol::ClassSummary {
    protocol::ClassSummary {
        qualified_name: ClassLiteral::Static(class).qualified_name(db).to_string(),
        bases: class
            .explicit_bases(db)
            .iter()
            .map(|base| plugin_class_base_type_expr_from_type(db, *base))
            .collect(),
        decorators: summary.decorators.clone(),
        metaclass: summary
            .metaclass
            .map(|metaclass| plugin_type_expr_from_type(db, metaclass)),
        fields: summary
            .fields
            .iter()
            .map(|field| protocol::FieldSummary {
                name: field.name.to_string(),
                annotation: field
                    .annotation
                    .map(|ty| plugin_type_expr_from_type(db, ty)),
                assigned_value: field.assigned_value.clone(),
                inferred_type: field
                    .inferred_type
                    .map(|ty| plugin_type_expr_from_type(db, ty)),
                has_default: field.has_default,
                source: field.source.clone(),
            })
            .collect(),
        methods: summary.methods.clone(),
        nested_classes: summary.nested_classes.clone(),
        class_constants: summary.class_constants.clone(),
        source: summary.source.clone(),
    }
}

fn plugin_method_summary(
    db: &dyn Db,
    file: File,
    _module: &ParsedModuleRef,
    statement: &ast::Stmt,
    owner_qualified_name: &str,
) -> Option<protocol::MethodSummary> {
    let function = statement.as_function_def_stmt()?;
    let definition = semantic_index(db, file).expect_single_definition(function);
    let parameters = &function.parameters;
    let mut summarized_parameters = Vec::with_capacity(parameters.len());

    let mut push_parameter =
        |parameter: &ast::Parameter, kind: protocol::ParameterKind, required: bool| {
            summarized_parameters.push(protocol::Parameter {
                name: Some(parameter.name.to_string()),
                kind,
                type_expr: parameter.annotation.as_deref().map(|annotation| {
                    plugin_type_expr_from_type(
                        db,
                        definition_expression_type(db, definition, annotation),
                    )
                }),
                required,
            });
        };

    for parameter in &parameters.posonlyargs {
        push_parameter(
            &parameter.parameter,
            protocol::ParameterKind::PositionalOnly,
            parameter.default.is_none(),
        );
    }
    for parameter in &parameters.args {
        push_parameter(
            &parameter.parameter,
            protocol::ParameterKind::PositionalOrKeyword,
            parameter.default.is_none(),
        );
    }
    if let Some(parameter) = parameters.vararg.as_deref() {
        push_parameter(parameter, protocol::ParameterKind::VarArgs, false);
    }
    for parameter in &parameters.kwonlyargs {
        push_parameter(
            &parameter.parameter,
            protocol::ParameterKind::KeywordOnly,
            parameter.default.is_none(),
        );
    }
    if let Some(parameter) = parameters.kwarg.as_deref() {
        push_parameter(parameter, protocol::ParameterKind::Kwargs, false);
    }

    let qualified_name = format!("{owner_qualified_name}.{}", function.name);
    Some(protocol::MethodSummary {
        name: function.name.to_string(),
        decorators: function
            .decorator_list
            .iter()
            .map(|decorator| plugin_call_or_symbol_summary(db, definition, &decorator.expression))
            .collect(),
        parameters: summarized_parameters,
        return_type: function.returns.as_deref().map(|returns| {
            let is_self = plugin_symbol_ref_from_expr(returns).is_some_and(|symbol| {
                symbol.qualified_name == "Self" || symbol.qualified_name.ends_with(".Self")
            });
            if is_self {
                let bound = protocol::TypeExpr::annotation(owner_qualified_name);
                protocol::TypeExpr::annotation("Self").with_snapshot(
                    protocol::TypeSnapshot::SelfType {
                        bound: Some(Box::new(protocol::TypeSnapshot::expression(&bound))),
                    },
                )
            } else {
                plugin_type_expr_from_type(db, definition_expression_type(db, definition, returns))
            }
        }),
        is_public: !function.name.starts_with('_'),
        source: plugin_symbol_source(db, file, function.range(), Some(qualified_name)),
    })
}

fn plugin_class_base_type_expr_from_type<'db>(
    db: &'db dyn Db,
    ty: Type<'db>,
) -> protocol::TypeExpr {
    let Type::GenericAlias(alias) = ty else {
        return plugin_type_expr_from_type(db, ty);
    };
    let origin = ClassLiteral::Static(alias.origin(db))
        .qualified_name(db)
        .to_string();
    let arguments = alias
        .specialization(db)
        .types(db)
        .iter()
        .map(|argument| plugin_type_expr_from_type(db, *argument).expression)
        .collect::<Vec<_>>();
    let mut summary = plugin_type_expr_from_type(db, ty);
    summary.expression = format!("{origin}[{}]", arguments.join(", "));
    summary
}

fn nested_class_summary(
    db: &dyn Db,
    file: File,
    module: &ParsedModuleRef,
    owner_qualified_name: &str,
    statement: &ast::Stmt,
) -> Option<protocol::NestedClassSummary> {
    let class = statement.as_class_def_stmt()?;
    let qualified_name = format!("{owner_qualified_name}.{}", class.name);
    let definition = semantic_index(db, file).expect_single_definition(class);

    Some(protocol::NestedClassSummary {
        name: class.name.to_string(),
        qualified_name: qualified_name.clone(),
        bases: class
            .arguments
            .as_ref()
            .map(|arguments| {
                arguments
                    .args
                    .iter()
                    .map(|base| {
                        plugin_class_base_type_expr_from_type(
                            db,
                            definition_expression_type(db, definition, base),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        class_constants: class
            .body
            .iter()
            .filter_map(|statement| {
                constant_summary_from_statement(db, file, module, statement, Some(&qualified_name))
            })
            .collect(),
        source: plugin_symbol_source(db, file, class.range(), Some(qualified_name)),
    })
}

fn constant_summary_from_statement(
    db: &dyn Db,
    file: File,
    _module: &ParsedModuleRef,
    statement: &ast::Stmt,
    owner_qualified_name: Option<&str>,
) -> Option<protocol::ConstantSummary> {
    let (name, value, target_range) = match statement {
        ast::Stmt::Assign(assign) => {
            let target = assign.targets.first()?.as_name_expr()?;
            (target.id.to_string(), assign.value.as_ref(), target.range())
        }
        ast::Stmt::AnnAssign(assign) => {
            let target = assign.target.as_name_expr()?;
            let value = assign.value.as_deref()?;
            (target.id.to_string(), value, target.range())
        }
        _ => return None,
    };

    let value_summary = plugin_literal_value_from_expr(value);
    if value_summary.is_unknown() {
        return None;
    }

    Some(protocol::ConstantSummary {
        name: name.clone(),
        value: value_summary,
        type_expr: None,
        source: plugin_symbol_source(
            db,
            file,
            target_range,
            owner_qualified_name.map(|owner| format!("{owner}.{name}")),
        ),
    })
}

fn plugin_assigned_value_summary<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    value: &ast::Expr,
    inferred_type: Option<Type<'db>>,
) -> protocol::AssignedValueSummary {
    if let ast::Expr::Call(call) = value
        && let Some(callee) = plugin_symbol_ref_from_expr(&call.func)
    {
        return protocol::AssignedValueSummary::Call(protocol::CallValueSummary {
            callee,
            receiver: plugin_call_receiver_summary(db, definition, &call.func),
            arguments: plugin_argument_summaries(db, definition, &call.arguments),
            return_type: inferred_type.map(|ty| plugin_type_expr_from_type(db, ty)),
        });
    }

    let literal = plugin_literal_value_from_expr(value);
    if !literal.is_unknown() {
        return protocol::AssignedValueSummary::Literal { value: literal };
    }

    if let Some(symbol) = plugin_symbol_ref_from_expr(value) {
        return match value {
            ast::Expr::Attribute(_) => protocol::AssignedValueSummary::Attribute(symbol),
            ast::Expr::Name(_) => protocol::AssignedValueSummary::Name(symbol),
            _ => protocol::AssignedValueSummary::Other {
                inferred_type: inferred_type.map(|ty| plugin_type_expr_from_type(db, ty)),
            },
        };
    }

    protocol::AssignedValueSummary::Other {
        inferred_type: inferred_type.map(|ty| plugin_type_expr_from_type(db, ty)),
    }
}

fn plugin_call_or_symbol_summary<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    expression: &ast::Expr,
) -> protocol::CallOrSymbolSummary {
    if let ast::Expr::Call(call) = expression
        && let Some(callee) = plugin_symbol_ref_from_expr(&call.func)
    {
        return protocol::CallOrSymbolSummary::Call(protocol::CallValueSummary {
            callee,
            receiver: plugin_call_receiver_summary(db, definition, &call.func),
            arguments: plugin_argument_summaries(db, definition, &call.arguments),
            return_type: Some(plugin_type_expr_from_type(
                db,
                definition_expression_type(db, definition, expression),
            )),
        });
    }

    if let Some(symbol) = plugin_symbol_ref_from_expr(expression) {
        return protocol::CallOrSymbolSummary::Symbol(symbol);
    }

    protocol::CallOrSymbolSummary::Other {
        inferred_type: Some(plugin_type_expr_from_type(
            db,
            definition_expression_type(db, definition, expression),
        )),
    }
}

fn plugin_call_receiver_summary<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    callee: &ast::Expr,
) -> Option<protocol::ValueSummary> {
    let ast::Expr::Attribute(attribute) = callee else {
        return None;
    };
    Some(protocol::ValueSummary {
        symbol: plugin_symbol_ref_from_expr(&attribute.value),
        type_expr: Some(plugin_type_expr_from_type(
            db,
            definition_expression_type(db, definition, &attribute.value),
        )),
    })
}

fn plugin_argument_summaries<'db>(
    db: &'db dyn Db,
    definition: Definition<'db>,
    arguments: &ast::Arguments,
) -> Vec<protocol::ArgumentSummary> {
    let mut summaries = Vec::with_capacity(arguments.len());

    for argument in &arguments.args {
        let kind = if argument.is_starred_expr() {
            protocol::ArgumentKind::StarArgs
        } else {
            protocol::ArgumentKind::Positional
        };
        summaries.push(protocol::ArgumentSummary {
            name: None,
            kind,
            type_expr: Some(plugin_type_expr_from_type(
                db,
                definition_expression_type(db, definition, argument),
            )),
            value: plugin_literal_value_from_expr(argument),
            source: Some(plugin_symbol_source(
                db,
                definition.file(db),
                argument.range(),
                None,
            )),
        });
    }

    for keyword in &arguments.keywords {
        summaries.push(protocol::ArgumentSummary {
            name: keyword.arg.as_ref().map(|arg| arg.as_str().to_string()),
            kind: if keyword.arg.is_some() {
                protocol::ArgumentKind::Keyword
            } else {
                protocol::ArgumentKind::StarKwargs
            },
            type_expr: Some(plugin_type_expr_from_type(
                db,
                definition_expression_type(db, definition, &keyword.value),
            )),
            value: plugin_literal_value_from_expr(&keyword.value),
            source: Some(plugin_symbol_source(
                db,
                definition.file(db),
                keyword.range(),
                None,
            )),
        });
    }

    summaries
}

fn plugin_literal_value_from_expr(expression: &ast::Expr) -> protocol::LiteralValue {
    match expression {
        ast::Expr::BooleanLiteral(boolean) => protocol::LiteralValue::Bool {
            value: boolean.value,
        },
        ast::Expr::NoneLiteral(_) => protocol::LiteralValue::None,
        ast::Expr::StringLiteral(string) => protocol::LiteralValue::Str {
            value: string.value.to_str().to_string(),
        },
        ast::Expr::NumberLiteral(number) => match &number.value {
            ast::Number::Int(int) => int
                .as_i64()
                .map_or(protocol::LiteralValue::Unknown, |value| {
                    protocol::LiteralValue::Int { value }
                }),
            ast::Number::Float(_) | ast::Number::Complex { .. } => protocol::LiteralValue::Unknown,
        },
        ast::Expr::Name(_) => plugin_symbol_ref_from_expr(expression).map_or(
            protocol::LiteralValue::Unknown,
            protocol::LiteralValue::SymbolRef,
        ),
        ast::Expr::Attribute(_) => plugin_symbol_ref_from_expr(expression).map_or(
            protocol::LiteralValue::Unknown,
            protocol::LiteralValue::EnumRef,
        ),
        ast::Expr::Tuple(tuple) => protocol::LiteralValue::Tuple {
            items: tuple
                .elts
                .iter()
                .map(plugin_literal_value_from_expr)
                .collect(),
        },
        ast::Expr::List(list) => protocol::LiteralValue::List {
            items: list
                .elts
                .iter()
                .map(plugin_literal_value_from_expr)
                .collect(),
        },
        ast::Expr::Dict(dict) => protocol::LiteralValue::Dict {
            entries: dict
                .items
                .iter()
                .filter_map(|item| {
                    Some(protocol::LiteralDictEntry {
                        key: plugin_literal_value_from_expr(item.key.as_ref()?),
                        value: plugin_literal_value_from_expr(&item.value),
                    })
                })
                .collect(),
        },
        ast::Expr::BinOp(binary) if binary.op == ast::Operator::Add => plugin_literal_add(
            plugin_literal_value_from_expr(&binary.left),
            plugin_literal_value_from_expr(&binary.right),
        ),
        _ => protocol::LiteralValue::Unknown,
    }
}

fn plugin_settings_literal_value_from_expr(
    db: &dyn Db,
    module_name: &str,
    expression: &ast::Expr,
    settings_bindings: &BTreeMap<String, &ast::Expr>,
    import_bindings: &PluginSettingsImportBindings,
    resolving: &mut BTreeSet<String>,
    dependencies: &mut BTreeSet<String>,
) -> protocol::LiteralValue {
    match expression {
        ast::Expr::Name(name) => {
            let name = name.id.as_str();
            if let Some(value) = settings_bindings.get(name).copied() {
                let resolving_key = format!("{module_name}.{name}");
                if !resolving.insert(resolving_key.clone()) {
                    return protocol::LiteralValue::Unknown;
                }
                let resolved = plugin_settings_literal_value_from_expr(
                    db,
                    module_name,
                    value,
                    settings_bindings,
                    import_bindings,
                    resolving,
                    dependencies,
                );
                resolving.remove(&resolving_key);
                return resolved;
            }

            let Some((imported_module, imported_name)) = import_bindings.values.get(name) else {
                return protocol::LiteralValue::Unknown;
            };
            plugin_imported_settings_literal_value(
                db,
                imported_module,
                imported_name,
                resolving,
                dependencies,
            )
        }
        ast::Expr::Attribute(attribute) => {
            let ast::Expr::Name(module_alias) = attribute.value.as_ref() else {
                return plugin_literal_value_from_expr(expression);
            };
            let Some(imported_module) = import_bindings.modules.get(module_alias.id.as_str())
            else {
                return plugin_literal_value_from_expr(expression);
            };
            let imported_name = attribute.attr.as_str();
            if !is_static_settings_name(imported_name) {
                return protocol::LiteralValue::Unknown;
            }
            plugin_imported_settings_literal_value(
                db,
                imported_module,
                imported_name,
                resolving,
                dependencies,
            )
        }
        ast::Expr::Tuple(tuple) => protocol::LiteralValue::Tuple {
            items: tuple
                .elts
                .iter()
                .map(|item| {
                    plugin_settings_literal_value_from_expr(
                        db,
                        module_name,
                        item,
                        settings_bindings,
                        import_bindings,
                        resolving,
                        dependencies,
                    )
                })
                .collect(),
        },
        ast::Expr::List(list) => protocol::LiteralValue::List {
            items: list
                .elts
                .iter()
                .map(|item| {
                    plugin_settings_literal_value_from_expr(
                        db,
                        module_name,
                        item,
                        settings_bindings,
                        import_bindings,
                        resolving,
                        dependencies,
                    )
                })
                .collect(),
        },
        ast::Expr::Dict(dict) => protocol::LiteralValue::Dict {
            entries: dict
                .items
                .iter()
                .filter_map(|item| {
                    Some(protocol::LiteralDictEntry {
                        key: plugin_settings_literal_value_from_expr(
                            db,
                            module_name,
                            item.key.as_ref()?,
                            settings_bindings,
                            import_bindings,
                            resolving,
                            dependencies,
                        ),
                        value: plugin_settings_literal_value_from_expr(
                            db,
                            module_name,
                            &item.value,
                            settings_bindings,
                            import_bindings,
                            resolving,
                            dependencies,
                        ),
                    })
                })
                .collect(),
        },
        ast::Expr::BinOp(binary) if binary.op == ast::Operator::Add => plugin_literal_add(
            plugin_settings_literal_value_from_expr(
                db,
                module_name,
                &binary.left,
                settings_bindings,
                import_bindings,
                resolving,
                dependencies,
            ),
            plugin_settings_literal_value_from_expr(
                db,
                module_name,
                &binary.right,
                settings_bindings,
                import_bindings,
                resolving,
                dependencies,
            ),
        ),
        _ => plugin_literal_value_from_expr(expression),
    }
}

fn plugin_imported_settings_literal_value(
    db: &dyn Db,
    module_name: &str,
    setting_name: &str,
    resolving: &mut BTreeSet<String>,
    dependencies: &mut BTreeSet<String>,
) -> protocol::LiteralValue {
    let resolving_key = format!("{module_name}.{setting_name}");
    if !resolving.insert(resolving_key.clone()) {
        return protocol::LiteralValue::Unknown;
    }
    let Some(file) = plugin_settings_module_file(db, module_name) else {
        resolving.remove(&resolving_key);
        return protocol::LiteralValue::Unknown;
    };
    dependencies.insert(file.path(db).to_string());

    let parsed = parsed_module(db, file).load(db);
    let settings_bindings = parsed
        .syntax()
        .body
        .iter()
        .filter_map(setting_assignment_from_statement)
        .filter(|(name, _, _)| is_static_settings_name(name))
        .map(|(name, value, _)| (name, value))
        .collect::<BTreeMap<_, _>>();
    let Some(value) = settings_bindings.get(setting_name).copied() else {
        resolving.remove(&resolving_key);
        return protocol::LiteralValue::Unknown;
    };
    let import_bindings = plugin_settings_import_bindings(parsed.syntax().body.as_slice());
    let resolved = plugin_settings_literal_value_from_expr(
        db,
        module_name,
        value,
        &settings_bindings,
        &import_bindings,
        resolving,
        dependencies,
    );
    resolving.remove(&resolving_key);
    resolved
}

fn plugin_literal_add(
    left: protocol::LiteralValue,
    right: protocol::LiteralValue,
) -> protocol::LiteralValue {
    match (left, right) {
        (
            protocol::LiteralValue::Str { value: left },
            protocol::LiteralValue::Str { value: right },
        ) => protocol::LiteralValue::Str {
            value: format!("{left}{right}"),
        },
        (
            protocol::LiteralValue::List { items: mut left },
            protocol::LiteralValue::List { items: right },
        ) => {
            left.extend(right);
            protocol::LiteralValue::List { items: left }
        }
        (
            protocol::LiteralValue::Tuple { items: mut left },
            protocol::LiteralValue::Tuple { items: right },
        ) => {
            left.extend(right);
            protocol::LiteralValue::Tuple { items: left }
        }
        (
            protocol::LiteralValue::Int { value: left },
            protocol::LiteralValue::Int { value: right },
        ) => left
            .checked_add(right)
            .map_or(protocol::LiteralValue::Unknown, |value| {
                protocol::LiteralValue::Int { value }
            }),
        _ => protocol::LiteralValue::Unknown,
    }
}

fn plugin_symbol_ref_from_expr(expression: &ast::Expr) -> Option<protocol::SymbolRef> {
    match expression {
        ast::Expr::Name(name) => Some(protocol::SymbolRef {
            qualified_name: name.id.to_string(),
        }),
        ast::Expr::Attribute(attribute) => {
            let mut qualified_name = plugin_symbol_ref_from_expr(&attribute.value)?.qualified_name;
            qualified_name.push('.');
            qualified_name.push_str(attribute.attr.as_str());
            Some(protocol::SymbolRef { qualified_name })
        }
        _ => None,
    }
}

fn plugin_symbol_source(
    db: &dyn Db,
    file: File,
    range: TextRange,
    qualified_name: Option<String>,
) -> protocol::SymbolSource {
    let source = source_text(db, file);
    let index = line_index(db, file);
    let start = index.line_column(range.start(), source.as_str());
    let end = index.line_column(range.end(), source.as_str());

    protocol::SymbolSource {
        module: Some(plugin_module_name(db, file)),
        qualified_name,
        file_path: Some(file.path(db).to_string()),
        start: Some(protocol::TextPosition {
            line: u32::try_from(start.line.get()).unwrap_or(u32::MAX),
            column: u32::try_from(start.column.get()).unwrap_or(u32::MAX),
        }),
        end: Some(protocol::TextPosition {
            line: u32::try_from(end.line.get()).unwrap_or(u32::MAX),
            column: u32::try_from(end.column.get()).unwrap_or(u32::MAX),
        }),
    }
}

fn plugin_module_name(db: &dyn Db, file: File) -> String {
    ty_module_resolver::file_to_module(db, file)
        .map(|module| module.name(db).to_string())
        .unwrap_or_default()
}

fn plugin_resolve_member_request<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    member_name: &str,
    scope: PluginMemberScope,
    existing_ty: Option<Type<'db>>,
    owner_override: Option<Type<'db>>,
    project_index: Option<serde_json::Value>,
) -> protocol::PluginRequest {
    let file = class.file(db);
    let owner = owner_override.unwrap_or_else(|| match scope {
        PluginMemberScope::Class => Type::ClassLiteral(ClassLiteral::Static(class)),
        PluginMemberScope::Instance => ClassLiteral::Static(class).to_non_generic_instance(db),
    });
    let request = protocol::ResolveMemberRequest {
        context: plugin_semantic_context(db, file, false),
        owner: plugin_type_expr_from_type(db, owner),
        member_name: member_name.to_string(),
        existing_member: existing_ty.map(|ty| protocol::MemberSummary {
            name: member_name.to_string(),
            access: protocol::MemberAccessPatch::value(plugin_type_expr_from_type(db, ty)),
            is_read_only: false,
        }),
        project_index,
    };

    match scope {
        PluginMemberScope::Class => protocol::PluginRequest::ResolveClassMember(request),
        PluginMemberScope::Instance => protocol::PluginRequest::ResolveInstanceMember(request),
    }
}

fn mock_plugin_execute_class_transform(
    request: &protocol::PluginRequest,
) -> protocol::PluginResponse {
    let protocol::PluginRequest::AnalyzeClass(request) = request else {
        return protocol::PluginResponse::NoChange;
    };

    protocol::PluginResponse::ClassPatch(protocol::ClassPatch {
        fields: request
            .class
            .fields
            .iter()
            .filter_map(|field| {
                let type_expr = field.annotation.clone()?;
                Some(protocol::FieldPatch {
                    name: field.name.clone(),
                    mode: protocol::MemberPatchMode::FillOnMiss,
                    descriptor: None,
                    instance_get_type: type_expr.clone(),
                    instance_set_type: Some(type_expr.clone()),
                    constructor_parameter: Some(protocol::Parameter {
                        name: Some(field.name.clone()),
                        kind: protocol::ParameterKind::KeywordOnly,
                        type_expr: Some(type_expr),
                        required: !field.has_default,
                    }),
                    has_default: field.has_default,
                })
            })
            .collect(),
        class_members: Vec::new(),
        instance_members: Vec::new(),
        constructor: None,
        diagnostics: Vec::new(),
    })
}

fn mock_plugin_execute_project_index(
    request: &protocol::PluginRequest,
) -> protocol::PluginResponse {
    let protocol::PluginRequest::BuildProjectIndex(_) = request else {
        return protocol::PluginResponse::NoChange;
    };

    protocol::PluginResponse::ProjectIndex(protocol::ProjectIndexResponse {
        plugin_index: serde_json::Value::Null,
        contributions: Vec::new(),
        virtual_types: Vec::new(),
        dependencies: Vec::new(),
        diagnostics: Vec::new(),
    })
}

fn execute_project_index_plugin(
    db: &dyn Db,
    plugin: &SemanticPlugin,
    request: &protocol::PluginRequest,
) -> protocol::PluginResponse {
    match plugin.runtime() {
        SemanticPluginRuntime::Mock => mock_plugin_execute_project_index(request),
        SemanticPluginRuntime::InProcess | SemanticPluginRuntime::Wasm => db
            .execute_semantic_plugin(plugin.id(), request)
            .unwrap_or_else(|error| {
                tracing::warn!(
                    plugin_id = plugin.id(),
                    error = error.message(),
                    hint = error.hint(),
                    "plugin project-index hook failed; falling back to no index"
                );
                protocol::PluginResponse::NoChange
            }),
    }
}

fn execute_class_transform_plugin(
    db: &dyn Db,
    plugin: &SemanticPlugin,
    request: &protocol::PluginRequest,
) -> protocol::PluginResponse {
    match plugin.runtime() {
        SemanticPluginRuntime::Mock => mock_plugin_execute_class_transform(request),
        SemanticPluginRuntime::InProcess | SemanticPluginRuntime::Wasm => db
            .execute_semantic_plugin(plugin.id(), request)
            .unwrap_or_else(|error| {
                tracing::warn!(
                    plugin_id = plugin.id(),
                    error = error.message(),
                    hint = error.hint(),
                    "plugin class-transform hook failed; falling back to no change"
                );
                protocol::PluginResponse::NoChange
            }),
    }
}

fn mock_plugin_execute_member(request: &protocol::PluginRequest) -> protocol::PluginResponse {
    let (protocol::PluginRequest::ResolveClassMember(request)
    | protocol::PluginRequest::ResolveInstanceMember(request)) = request
    else {
        return protocol::PluginResponse::NoChange;
    };

    protocol::PluginResponse::MemberPatch(protocol::MemberPatch {
        name: request.member_name.clone(),
        mode: protocol::MemberPatchMode::FillOnMiss,
        access: protocol::MemberAccessPatch::value(protocol::TypeExpr {
            expression: "str".to_string(),
            imports: Vec::new(),
            mode: protocol::TypeExprMode::Annotation,
            snapshot: None,
        }),
        read_only: false,
        diagnostics: Vec::new(),
    })
}

fn execute_member_plugin(
    db: &dyn Db,
    plugin: &SemanticPlugin,
    request: &protocol::PluginRequest,
) -> protocol::PluginResponse {
    match plugin.runtime() {
        SemanticPluginRuntime::Mock => mock_plugin_execute_member(request),
        SemanticPluginRuntime::InProcess | SemanticPluginRuntime::Wasm => db
            .execute_semantic_plugin(plugin.id(), request)
            .unwrap_or_else(|error| {
                tracing::warn!(
                    plugin_id = plugin.id(),
                    error = error.message(),
                    hint = error.hint(),
                    "plugin member hook failed; falling back to no change"
                );
                protocol::PluginResponse::NoChange
            }),
    }
}

fn merge_plugin_class_response<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    response: protocol::PluginResponse,
    virtual_types: &[PluginVirtualTypePatch<'db>],
    fields: &mut Vec<PluginClassFieldPatch<'db>>,
    class_members: &mut Vec<PluginMemberPatch<'db>>,
    instance_members: &mut Vec<PluginMemberPatch<'db>>,
    constructor: &mut Option<PluginConstructorPatch<'db>>,
) {
    let protocol::PluginResponse::ClassPatch(patch) = response else {
        return;
    };

    for field in patch.fields {
        let name = Name::new(&field.name);
        if fields.iter().any(|existing| existing.name == name) {
            continue;
        }
        let descriptor_class_ty = match field.descriptor.as_ref() {
            Some(protocol::MemberAccessPatch::Value { type_expr }) => {
                Some(plugin_type_expr_to_type_in_class_with_virtual_types(
                    db,
                    type_expr,
                    class,
                    virtual_types,
                ))
            }
            Some(protocol::MemberAccessPatch::Descriptor { class_type, .. }) => {
                class_type.as_ref().map(|type_expr| {
                    plugin_type_expr_to_type_in_class_with_virtual_types(
                        db,
                        type_expr,
                        class,
                        virtual_types,
                    )
                })
            }
            Some(protocol::MemberAccessPatch::Callable { fallback_type, .. }) => {
                Some(plugin_type_expr_to_type_in_class_with_virtual_types(
                    db,
                    fallback_type,
                    class,
                    virtual_types,
                ))
            }
            None => None,
        };
        fields.push(PluginClassFieldPatch {
            name,
            replace_existing: field.mode == protocol::MemberPatchMode::ReplaceExisting,
            descriptor_class_ty,
            instance_get_ty: plugin_type_expr_to_type_in_class_with_virtual_types(
                db,
                &field.instance_get_type,
                class,
                virtual_types,
            ),
            instance_set_ty: field.instance_set_type.as_ref().map(|type_expr| {
                plugin_type_expr_to_type_in_class_with_virtual_types(
                    db,
                    type_expr,
                    class,
                    virtual_types,
                )
            }),
            has_default: field.has_default,
            constructor_parameter: field.constructor_parameter.map(|parameter| {
                plugin_constructor_parameter_from_protocol(db, class, parameter, virtual_types)
            }),
        });
    }

    for member in patch.class_members {
        let name = Name::new(&member.name);
        if class_members.iter().any(|existing| existing.name == name) {
            continue;
        }
        class_members.push(PluginMemberPatch {
            name,
            replace_existing: member.mode == protocol::MemberPatchMode::ReplaceExisting,
            ty: plugin_member_access_to_type_in_class(db, &member.access, class, virtual_types),
            read_only: member.read_only,
        });
    }

    for member in patch.instance_members {
        let name = Name::new(&member.name);
        if instance_members
            .iter()
            .any(|existing| existing.name == name)
        {
            continue;
        }
        instance_members.push(PluginMemberPatch {
            name,
            replace_existing: member.mode == protocol::MemberPatchMode::ReplaceExisting,
            ty: plugin_member_access_to_type_in_class(db, &member.access, class, virtual_types),
            read_only: member.read_only,
        });
    }

    if constructor.is_none() {
        *constructor = patch.constructor.map(|signature| PluginConstructorPatch {
            parameters: signature
                .parameters
                .into_iter()
                .map(|parameter| {
                    plugin_constructor_parameter_from_protocol(db, class, parameter, virtual_types)
                })
                .collect(),
        });
    }
}

fn plugin_constructor_parameter_from_protocol<'db>(
    db: &'db dyn Db,
    class: StaticClassLiteral<'db>,
    parameter: protocol::Parameter,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> PluginConstructorParameter<'db> {
    PluginConstructorParameter {
        name: parameter.name.map(Name::new),
        kind: match parameter.kind {
            protocol::ParameterKind::PositionalOnly => {
                PluginConstructorParameterKind::PositionalOnly
            }
            protocol::ParameterKind::PositionalOrKeyword => {
                PluginConstructorParameterKind::PositionalOrKeyword
            }
            protocol::ParameterKind::VarArgs => PluginConstructorParameterKind::VarArgs,
            protocol::ParameterKind::KeywordOnly => PluginConstructorParameterKind::KeywordOnly,
            protocol::ParameterKind::Kwargs => PluginConstructorParameterKind::Kwargs,
        },
        ty: parameter
            .type_expr
            .as_ref()
            .map_or_else(Type::unknown, |type_expr| {
                plugin_type_expr_to_type_in_class_with_virtual_types(
                    db,
                    type_expr,
                    class,
                    virtual_types,
                )
            }),
        required: parameter.required,
    }
}

fn plugin_constructor_parameter_from_protocol_type_expr<'db>(
    db: &'db dyn Db,
    parameter: protocol::Parameter,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> PluginConstructorParameter<'db> {
    PluginConstructorParameter {
        name: parameter.name.map(Name::new),
        kind: match parameter.kind {
            protocol::ParameterKind::PositionalOnly => {
                PluginConstructorParameterKind::PositionalOnly
            }
            protocol::ParameterKind::PositionalOrKeyword => {
                PluginConstructorParameterKind::PositionalOrKeyword
            }
            protocol::ParameterKind::VarArgs => PluginConstructorParameterKind::VarArgs,
            protocol::ParameterKind::KeywordOnly => PluginConstructorParameterKind::KeywordOnly,
            protocol::ParameterKind::Kwargs => PluginConstructorParameterKind::Kwargs,
        },
        ty: parameter
            .type_expr
            .as_ref()
            .map_or_else(Type::unknown, |type_expr| {
                plugin_type_expr_to_type_with_virtual_types(db, type_expr, virtual_types)
            }),
        required: parameter.required,
    }
}

fn plugin_member_response_to_patch<'db>(
    db: &'db dyn Db,
    response: protocol::PluginResponse,
    expected_name: &Name,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> Option<PluginMemberPatch<'db>> {
    let protocol::PluginResponse::MemberPatch(member) = response else {
        return None;
    };

    if member.name != expected_name.as_str() {
        return None;
    }

    Some(PluginMemberPatch {
        name: Name::new(&member.name),
        replace_existing: member.mode == protocol::MemberPatchMode::ReplaceExisting,
        ty: plugin_member_access_to_type_with_virtual_types(db, &member.access, virtual_types),
        read_only: member.read_only,
    })
}

fn plugin_member_access_to_type_in_class<'db>(
    db: &'db dyn Db,
    access: &protocol::MemberAccessPatch,
    class: StaticClassLiteral<'db>,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> Type<'db> {
    match access {
        protocol::MemberAccessPatch::Callable { signature, .. } => {
            plugin_callable_type_from_protocol_signature_in_class(
                db,
                signature,
                class,
                virtual_types,
            )
        }
        _ => plugin_type_expr_to_type_in_class_with_virtual_types(
            db,
            access.instance_get_type(),
            class,
            virtual_types,
        ),
    }
}

fn plugin_member_access_to_type_with_virtual_types<'db>(
    db: &'db dyn Db,
    access: &protocol::MemberAccessPatch,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> Type<'db> {
    match access {
        protocol::MemberAccessPatch::Callable { signature, .. } => {
            plugin_callable_type_from_protocol_signature_with_virtual_types(
                db,
                signature,
                virtual_types,
            )
        }
        _ => plugin_type_expr_to_type_with_virtual_types(
            db,
            access.instance_get_type(),
            virtual_types,
        ),
    }
}

fn plugin_member_to_member<'db>(member: &PluginMemberPatch<'db>) -> Member<'db> {
    let qualifiers = if member.read_only {
        TypeQualifiers::READ_ONLY
    } else {
        TypeQualifiers::empty()
    };

    Member {
        inner: Place::declared(member.ty).with_qualifiers(qualifiers),
    }
}

impl<'db> PluginContributionMemberPatch<'db> {
    fn member_name(&self) -> Option<&Name> {
        match self {
            Self::Member(member) => Some(&member.name),
            Self::Field(field) => Some(&field.name),
            Self::Constructor(_) => None,
        }
    }

    fn sort_name(&self) -> &str {
        match self {
            Self::Member(member) => member.name.as_str(),
            Self::Field(field) => field.name.as_str(),
            Self::Constructor(_) => "__init__",
        }
    }

    fn replaces_existing(&self) -> bool {
        match self {
            Self::Member(member) => member.replace_existing,
            Self::Field(field) => field.replace_existing,
            Self::Constructor(_) => false,
        }
    }
}

fn plugin_contribution_to_member<'db>(
    patch: &PluginContributionMemberPatch<'db>,
    scope: PluginMemberScope,
) -> Option<Member<'db>> {
    match patch {
        PluginContributionMemberPatch::Member(member) => Some(plugin_member_to_member(member)),
        PluginContributionMemberPatch::Field(field) => match scope {
            PluginMemberScope::Class => field.descriptor_class_ty.map(Member::definitely_declared),
            PluginMemberScope::Instance => Some(Member::definitely_declared(field.instance_get_ty)),
        },
        PluginContributionMemberPatch::Constructor(_) => None,
    }
}

fn plugin_field_constructor_parameter<'db>(
    field: &PluginClassFieldPatch<'db>,
) -> Option<Parameter<'db>> {
    let constructor_parameter = field.constructor_parameter.as_ref()?;
    let mut parameter = plugin_signature_parameter(constructor_parameter)?;
    let parameter_ty = if constructor_parameter.ty.is_unknown() {
        field.instance_set_ty.unwrap_or(field.instance_get_ty)
    } else {
        constructor_parameter.ty
    };
    parameter = parameter.with_annotated_type(parameter_ty);
    if !constructor_parameter.required || field.has_default {
        Some(parameter.with_default_type(parameter_ty))
    } else {
        Some(parameter)
    }
}

fn plugin_signature_parameter<'db>(
    parameter: &PluginConstructorParameter<'db>,
) -> Option<Parameter<'db>> {
    let signature_parameter = match parameter.kind {
        PluginConstructorParameterKind::PositionalOnly => {
            Parameter::positional_only(parameter.name.clone())
        }
        PluginConstructorParameterKind::PositionalOrKeyword => {
            Parameter::positional_or_keyword(parameter.name.clone()?)
        }
        PluginConstructorParameterKind::VarArgs => Parameter::variadic(
            parameter
                .name
                .clone()
                .unwrap_or_else(|| Name::new_static("args")),
        ),
        PluginConstructorParameterKind::KeywordOnly => {
            Parameter::keyword_only(parameter.name.clone()?)
        }
        PluginConstructorParameterKind::Kwargs => Parameter::keyword_variadic(
            parameter
                .name
                .clone()
                .unwrap_or_else(|| Name::new_static("kwargs")),
        ),
    }
    .with_annotated_type(parameter.ty);

    match parameter.kind {
        PluginConstructorParameterKind::PositionalOnly
        | PluginConstructorParameterKind::PositionalOrKeyword
        | PluginConstructorParameterKind::KeywordOnly
            if !parameter.required =>
        {
            Some(signature_parameter.with_default_type(parameter.ty))
        }
        _ => Some(signature_parameter),
    }
}

fn is_plugin_self_parameter(parameter: &PluginConstructorParameter<'_>) -> bool {
    parameter
        .name
        .as_ref()
        .is_some_and(|name| matches!(name.as_str(), "self" | "cls"))
}

#[salsa::tracked]
impl<'db> VarianceInferable<'db> for StaticClassLiteral<'db> {
    #[salsa::tracked(returns(copy), cycle_initial=|_, _, _, _| TypeVarVariance::Bivariant, heap_size=ruff_memory_usage::heap_size)]
    fn variance_of(self, db: &'db dyn Db, typevar: BoundTypeVarIdentity<'db>) -> TypeVarVariance {
        let typevar_in_generic_context = self
            .generic_context(db)
            .is_some_and(|generic_context| generic_context.contains(db, typevar));

        if !typevar_in_generic_context {
            return TypeVarVariance::Bivariant;
        }
        let class_body_scope = self.body_scope(db);

        let file = class_body_scope.file(db);
        let index = semantic_index(db, file);

        let explicit_bases_variances = self
            .explicit_bases(db)
            .iter()
            .map(|class| class.variance_of(db, typevar));

        let default_attribute_variance = {
            let is_namedtuple = CodeGeneratorKind::NamedTuple.matches(db, self.into());
            // Python 3.13 introduced a synthesized `__replace__` method on dataclasses which uses
            // their field types in contravariant position, thus meaning a frozen dataclass must
            // still be invariant in its field types. Other synthesized methods on dataclasses are
            // not considered here, since they don't use field types in their signatures. TODO:
            // ideally we'd have a single source of truth for information about synthesized
            // methods, so we just look them up normally and don't hardcode this knowledge here.
            let is_frozen_dataclass_prior_to_313 = Program::get(db).python_version(db)
                <= PythonVersion::PY312
                && CodeGeneratorKind::from_static_class(db, self)
                    .is_some_and(|kind| self.has_dataclass_param(db, kind, DataclassFlags::FROZEN));

            if is_namedtuple || is_frozen_dataclass_prior_to_313 {
                TypeVarVariance::Covariant
            } else {
                TypeVarVariance::Invariant
            }
        };

        let init_name: &Name = &"__init__".into();
        let new_name: &Name = &"__new__".into();

        let use_def_map = index.use_def_map(class_body_scope.file_scope_id(db));
        let table = place_table(db, class_body_scope);
        let attribute_places_and_qualifiers =
            use_def_map
                .all_end_of_scope_symbol_declarations()
                .map(|(symbol_id, declarations)| {
                    let place_and_qual =
                        place_from_declarations(db, declarations).ignore_conflicting_declarations();
                    (symbol_id, place_and_qual)
                })
                .chain(use_def_map.all_end_of_scope_symbol_bindings().map(
                    |(symbol_id, bindings)| {
                        (symbol_id, place_from_bindings(db, bindings).place.into())
                    },
                ))
                .filter_map(|(symbol_id, place_and_qual)| {
                    if let Some(name) = table.place(symbol_id).as_symbol().map(Symbol::name) {
                        (![init_name, new_name].contains(&name))
                            .then_some((name.to_string(), place_and_qual))
                    } else {
                        None
                    }
                });

        // Dataclasses can have some additional synthesized methods (`__eq__`, `__hash__`,
        // `__lt__`, etc.) but none of these will have field types type variables in their signatures, so we
        // don't need to consider them for variance.

        let attribute_names = attribute_scopes(db, self.body_scope(db))
            .flat_map(|function_scope_id| {
                index
                    .place_table(function_scope_id)
                    .members()
                    .filter_map(|member| member.as_instance_attribute())
                    .filter(|name| *name != init_name && *name != new_name)
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .dedup();

        let attribute_variances = attribute_names
            .map(|name| {
                let place_and_quals = self.own_instance_member(db, &name).inner;
                (name, place_and_quals)
            })
            .chain(attribute_places_and_qualifiers)
            .dedup()
            .filter_map(|(name, place_and_qual)| {
                place_and_qual.ignore_possibly_undefined().map(|ty| {
                    let variance = if place_and_qual
                        .qualifiers
                        // `CLASS_VAR || FINAL` is really `all()`, but
                        // we want to be robust against new qualifiers
                        .intersects(TypeQualifiers::CLASS_VAR | TypeQualifiers::FINAL)
                        // We don't allow mutation of methods or properties
                        || ty.is_function_literal()
                        || ty.is_property_instance()
                        // Underscore-prefixed attributes are assumed not to be externally mutated
                        || name.starts_with('_')
                    {
                        // CLASS_VAR: class vars generally shouldn't contain the
                        // type variable, but they could if it's a
                        // callable type. They can't be mutated on instances.
                        //
                        // FINAL: final attributes are immutable, and thus covariant
                        TypeVarVariance::Covariant
                    } else {
                        default_attribute_variance
                    };
                    ty.with_polarity(variance).variance_of(db, typevar)
                })
            });

        let extra_items_variance = TypedDictType::new(self.identity_specialization(db))
            .explicit_extra_items(db)
            .map(|extra_items| {
                let polarity = if extra_items.is_read_only() {
                    TypeVarVariance::Covariant
                } else {
                    TypeVarVariance::Invariant
                };
                extra_items
                    .declared_ty
                    .with_polarity(polarity)
                    .variance_of(db, typevar)
            });

        attribute_variances
            .chain(explicit_bases_variances)
            .chain(extra_items_variance)
            .collect()
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, get_size2::GetSize)]
pub(crate) enum InheritanceCycle {
    /// The class is cyclically defined and is a participant in the cycle.
    /// i.e., it inherits either directly or indirectly from itself.
    Participant,
    /// The class inherits from a class that is a `Participant` in an inheritance cycle,
    /// but is not itself a participant.
    Inherited,
}

impl InheritanceCycle {
    pub(crate) const fn is_participant(self) -> bool {
        matches!(self, InheritanceCycle::Participant)
    }
}

fn explicit_bases_cycle_initial<'db>(
    db: &'db dyn Db,
    id: salsa::Id,
    literal: StaticClassLiteral<'db>,
) -> Box<[Type<'db>]> {
    let module = parsed_module(db, literal.file(db)).load(db);
    let class_stmt = literal.node(db, &module);
    // Try to produce a list of `Divergent` types of the right length. However, if one or more of
    // the bases is a starred expression, we don't know how many entries that will eventually
    // expand to.
    vec![Type::divergent(id); class_stmt.bases().len()].into_boxed_slice()
}

fn explicit_bases_cycle_fn<'db>(
    db: &'db dyn Db,
    cycle: &salsa::Cycle,
    previous: &[Type<'db>],
    current: Box<[Type<'db>]>,
    _literal: StaticClassLiteral<'db>,
) -> Box<[Type<'db>]> {
    if previous.len() == current.len() {
        // As long as the length of bases hasn't changed, use the same "monotonic widening"
        // strategy that we use with most types, to avoid oscillations.
        current
            .iter()
            .zip(previous.iter())
            .map(|(curr, prev)| curr.cycle_normalized(db, *prev, cycle))
            .collect()
    } else {
        // The length of bases has changed, presumably because we expanded a starred expression. We
        // don't do "monotonic widening" here, because we don't want to make assumptions about
        // which previous entries correspond to which current ones. An oscillation here would be
        // unfortunate, but maybe only pathological programs can trigger such a thing.
        current
    }
}

#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
struct ImplicitAttributeName<'db> {
    #[returns(copy)]
    class_body_scope: ScopeId<'db>,
    #[returns(deref)]
    name: CompactString,
    #[returns(copy)]
    target_method_decorator: MethodDecorator,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for ImplicitAttributeName<'_> {}

#[salsa::tracked(returns(deref), heap_size=ruff_memory_usage::heap_size)]
fn implicit_attribute_names<'db>(db: &'db dyn Db, class_body_scope: ScopeId<'db>) -> Box<[Name]> {
    let index = semantic_index(db, class_body_scope.file(db));
    let mut names = Vec::new();

    for function_scope_id in attribute_scopes(db, class_body_scope) {
        names.extend(
            index
                .place_table(function_scope_id)
                .members()
                .filter_map(|member| member.as_instance_attribute().map(Name::new)),
        );
    }

    names.sort_unstable();
    names.dedup();
    names.into_boxed_slice()
}

fn implicit_attribute_cycle_recover<'db>(
    db: &'db dyn Db,
    cycle: &salsa::Cycle,
    previous_member: &Member<'db>,
    member: Member<'db>,
    _attribute: ImplicitAttributeName<'db>,
) -> Member<'db> {
    let inner = member
        .inner
        .cycle_normalized(db, previous_member.inner, cycle);
    Member { inner }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use crate::db::tests::{TestDb, TestDbBuilder};
    use crate::place::global_symbol;
    use ruff_db::files::system_path_to_file;
    use ruff_db::system::DbWithWritableSystem as _;
    use ty_plugin_examples::{
        MiniDjangoPlugin, ModelClassTransformPlugin, class_transform, minidjango,
    };
    use ty_plugin_sdk::Plugin as _;
    use ty_python_core::program::{
        ProgramSettings, SemanticPluginEnvironment, SemanticPluginMemberClaim,
        SemanticPluginMethodClaim,
    };
    use ty_python_core::semantic_index;

    fn static_class_literal<'db>(
        db: &'db TestDb,
        path: &str,
        class_name: &str,
    ) -> StaticClassLiteral<'db> {
        let file = system_path_to_file(db, path).expect("test file should exist");
        let module = parsed_module(db, file).load(db);
        let class_node = module
            .syntax()
            .body
            .iter()
            .find_map(|statement| {
                let class = statement.as_class_def_stmt()?;
                (class.name.as_str() == class_name).then_some(class)
            })
            .expect("test class should exist");
        let definition = semantic_index(db, file).expect_single_definition(class_node);
        let ClassLiteral::Static(class) =
            crate::types::infer::original_class_type(db, definition).expect("class type")
        else {
            panic!("expected static class");
        };
        class
    }

    fn class_transform_request<'db>(
        db: &'db TestDb,
        path: &str,
        class_name: &str,
    ) -> protocol::AnalyzeClassRequest {
        let class = static_class_literal(db, path, class_name);

        let summary = plugin_class_summary(db, class);
        let protocol::PluginRequest::AnalyzeClass(request) =
            plugin_analyze_class_request(db, class, &summary, None)
        else {
            panic!("expected AnalyzeClass request");
        };
        request
    }

    fn install_semantic_plugin(db: &mut TestDb, plugin: SemanticPlugin) {
        let current_program = Program::get(db);
        let settings = ProgramSettings {
            python_version: current_program.python_version_with_source(db).clone(),
            python_platform: current_program.python_platform(db).clone(),
            search_paths: current_program.search_paths(db).clone(),
            semantic_plugins: SemanticPluginEnvironment::new(1, [plugin]),
        };
        Program::init_or_update(db, settings);
    }

    fn write_minidjango_harness(db: &mut TestDb) -> anyhow::Result<()> {
        db.write_files([
            (
                "/src/minidjango.py",
                include_str!("../../../resources/plugin_fixtures/minidjango/minidjango.py"),
            ),
            (
                "/src/library.py",
                include_str!("../../../resources/plugin_fixtures/minidjango/library.py"),
            ),
            (
                "/src/accounts.py",
                include_str!("../../../resources/plugin_fixtures/minidjango/accounts.py"),
            ),
            (
                "/src/minidjango_settings.py",
                include_str!(
                    "../../../resources/plugin_fixtures/minidjango/minidjango_settings.py"
                ),
            ),
            (
                "/src/models.py",
                include_str!("../../../resources/plugin_fixtures/minidjango/models.py"),
            ),
        ])?;

        Ok(())
    }

    fn install_minidjango_plugin(db: &mut TestDb) {
        let sdk_plugin = MiniDjangoPlugin;
        let plugin_id = sdk_plugin.manifest().id;
        db.register_semantic_plugin_executor(plugin_id.clone(), move |request| {
            Ok(sdk_plugin.handle(request))
        });
        install_semantic_plugin(
            db,
            SemanticPlugin::new(
                plugin_id,
                SemanticPluginRuntime::InProcess,
                vec![minidjango::MODEL_BASE.to_string()],
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<String>::new(),
                Vec::<String>::new(),
            )
            .with_call_method_on_subclass_claims(
                Vec::<SemanticPluginMethodClaim>::new(),
                vec![
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "filter"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "get"),
                    SemanticPluginMethodClaim::on_subclass_of(
                        minidjango::MANAGER_BASE,
                        "get_or_create",
                    ),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "first"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "count"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "exists"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "values"),
                    SemanticPluginMethodClaim::on_subclass_of(
                        minidjango::MANAGER_BASE,
                        "values_list",
                    ),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "annotate"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "filter"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "get"),
                    SemanticPluginMethodClaim::on_subclass_of(
                        minidjango::QUERYSET_BASE,
                        "get_or_create",
                    ),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "first"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "count"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "exists"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "values"),
                    SemanticPluginMethodClaim::on_subclass_of(
                        minidjango::QUERYSET_BASE,
                        "values_list",
                    ),
                    SemanticPluginMethodClaim::on_subclass_of(
                        minidjango::QUERYSET_BASE,
                        "annotate",
                    ),
                ],
            )
            .with_settings_module_claims(vec!["minidjango_settings".to_string()])
            .with_project_index_enabled(true),
        );
    }

    #[test]
    fn class_transform_executes_sdk_plugin_through_in_process_runtime() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/toy.py",
            r#"
            class Model: ...
            "#,
        )?;
        db.write_dedented(
            "/src/models.py",
            r#"
            from toy import Model

            class Book(Model):
                title: str
                pages: int = 1
            "#,
        )?;

        let sdk_plugin = ModelClassTransformPlugin;
        let plugin_id = sdk_plugin.manifest().id;
        db.register_semantic_plugin_executor(plugin_id.clone(), move |request| {
            Ok(sdk_plugin.handle(request))
        });
        install_semantic_plugin(
            &mut db,
            SemanticPlugin::new(
                plugin_id,
                SemanticPluginRuntime::InProcess,
                vec![class_transform::MODEL_BASE.to_string()],
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<String>::new(),
                Vec::<String>::new(),
            ),
        );

        let class = static_class_literal(&db, "/src/models.py", "Book");
        let patch = class.plugin_class_transform_patch(&db);

        let title = patch
            .fields
            .iter()
            .find(|field| field.name.as_str() == "title")
            .expect("title field should come from SDK plugin");
        assert_eq!(title.instance_get_ty.display(&db).to_string(), "str");
        assert!(title.constructor_parameter.is_some());
        assert!(!title.has_default);

        let pages = patch
            .fields
            .iter()
            .find(|field| field.name.as_str() == "pages")
            .expect("pages field should come from SDK plugin");
        assert_eq!(pages.instance_get_ty.display(&db).to_string(), "int");
        assert!(pages.constructor_parameter.is_some());
        assert!(pages.has_default);

        let constructor = patch.constructor.as_ref().expect("constructor patch");
        let parameters = constructor.parameters.as_ref();
        assert_eq!(parameters.len(), 2);
        assert_eq!(parameters[0].name.as_ref().map(Name::as_str), Some("title"));
        assert_eq!(
            parameters[0].kind,
            PluginConstructorParameterKind::KeywordOnly
        );
        assert!(parameters[0].required);
        assert_eq!(parameters[1].name.as_ref().map(Name::as_str), Some("pages"));
        assert_eq!(
            parameters[1].kind,
            PluginConstructorParameterKind::KeywordOnly
        );
        assert!(!parameters[1].required);

        Ok(())
    }

    #[test]
    fn semantic_project_index_is_passed_to_later_hooks() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/toy.py",
            r#"
            class Model: ...
            "#,
        )?;
        db.write_dedented(
            "/src/models.py",
            r#"
            import toy

            def indexed_field():
                return object()

            class Model(toy.Model):
                pass

            def check(model: Model) -> None:
                good: str = model.indexed
                bad: int = model.indexed
                call_good: str = indexed_field()
                call_bad: int = indexed_field()
            "#,
        )?;

        let plugin_id = "example.project-index".to_string();
        db.register_semantic_plugin_executor(plugin_id.clone(), |request| match request {
            protocol::PluginRequest::BuildProjectIndex(request) => {
                assert!(
                    request
                        .classes
                        .iter()
                        .any(|class| class.qualified_name == "models.Model"),
                    "project index should receive real project class summaries: {request:#?}"
                );
                Ok(protocol::PluginResponse::ProjectIndex(
                    protocol::ProjectIndexResponse {
                        plugin_index: serde_json::json!({ "member_type": "str" }),
                        contributions: Vec::new(),
                        virtual_types: Vec::new(),
                        dependencies: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                ))
            }
            protocol::PluginRequest::AnalyzeClass(request) => {
                assert_eq!(
                    request
                        .project_index
                        .as_ref()
                        .and_then(|index| index.get("member_type")),
                    Some(&serde_json::json!("str"))
                );
                Ok(protocol::PluginResponse::ClassPatch(protocol::ClassPatch {
                    fields: Vec::new(),
                    class_members: Vec::new(),
                    instance_members: Vec::new(),
                    constructor: None,
                    diagnostics: Vec::new(),
                }))
            }
            protocol::PluginRequest::ResolveInstanceMember(request) => {
                assert_eq!(
                    request
                        .project_index
                        .as_ref()
                        .and_then(|index| index.get("member_type")),
                    Some(&serde_json::json!("str"))
                );
                Ok(protocol::PluginResponse::MemberPatch(
                    protocol::MemberPatch {
                        name: request.member_name.clone(),
                        mode: protocol::MemberPatchMode::FillOnMiss,
                        access: protocol::MemberAccessPatch::value(protocol::TypeExpr::annotation(
                            "str",
                        )),
                        read_only: true,
                        diagnostics: Vec::new(),
                    },
                ))
            }
            protocol::PluginRequest::AdjustCallReturn(request) => {
                assert_eq!(
                    request
                        .project_index
                        .as_ref()
                        .and_then(|index| index.get("member_type")),
                    Some(&serde_json::json!("str"))
                );
                Ok(protocol::PluginResponse::CallReturnPatch(
                    protocol::CallReturnPatch {
                        return_type: protocol::TypeExpr::annotation("str"),
                        diagnostics: Vec::new(),
                        result_metadata: None,
                    },
                ))
            }
            _ => Ok(protocol::PluginResponse::NoChange),
        });
        install_semantic_plugin(
            &mut db,
            SemanticPlugin::new(
                plugin_id,
                SemanticPluginRuntime::InProcess,
                vec!["toy.Model".to_string()],
                Vec::<SemanticPluginMemberClaim>::new(),
                vec![SemanticPluginMemberClaim::new("models.Model", "indexed")],
                Vec::<String>::new(),
                vec!["models.indexed_field".to_string()],
            )
            .with_project_index_enabled(true),
        );

        let class = static_class_literal(&db, "/src/models.py", "Model");
        let patch = class.plugin_class_transform_patch(&db);
        assert!(patch.fields.is_empty());

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let diagnostics = db.check_file(file);
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.primary_message())
            .collect::<Vec<_>>();
        assert!(
            messages
                .iter()
                .filter(|message| message.contains("not assignable"))
                .count()
                >= 2,
            "project-index-backed dynamic member and call hook should be typed as str: {messages:#?}"
        );

        Ok(())
    }

    #[test]
    fn semantic_project_index_applies_constructor_contributions() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/models.py",
            r#"
            class Widget:
                pass

            def check() -> None:
                Widget(name="ok")
                Widget(name=1)
            "#,
        )?;

        let plugin_id = "example.constructor-contribution".to_string();
        db.register_semantic_plugin_executor(plugin_id.clone(), |request| match request {
            protocol::PluginRequest::BuildProjectIndex(request) => {
                assert!(
                    request
                        .classes
                        .iter()
                        .any(|class| class.qualified_name == "models.Widget"),
                    "project index should receive Widget summary: {request:#?}"
                );
                Ok(protocol::PluginResponse::ProjectIndex(
                    protocol::ProjectIndexResponse {
                        plugin_index: serde_json::Value::Null,
                        contributions: vec![protocol::Contribution {
                            source: protocol::SymbolSource::default(),
                            target: protocol::ContributionTarget::Constructor {
                                qualified_name: "models.Widget".to_string(),
                            },
                            patch: protocol::ContributionPatch::Constructor(
                                protocol::CallableSignature {
                                    parameters: vec![protocol::Parameter {
                                        name: Some("name".to_string()),
                                        kind: protocol::ParameterKind::KeywordOnly,
                                        type_expr: Some(protocol::TypeExpr::annotation("str")),
                                        required: true,
                                    }],
                                    return_type: protocol::TypeExpr::annotation("None"),
                                },
                            ),
                            conflict_key: "models.Widget.__init__".to_string(),
                            diagnostics: Vec::new(),
                        }],
                        virtual_types: Vec::new(),
                        dependencies: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                ))
            }
            _ => Ok(protocol::PluginResponse::NoChange),
        });
        install_semantic_plugin(
            &mut db,
            SemanticPlugin::new(
                plugin_id,
                SemanticPluginRuntime::InProcess,
                Vec::<String>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<String>::new(),
                Vec::<String>::new(),
            )
            .with_project_index_enabled(true),
        );

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let diagnostics = db.check_file(file);
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.primary_message())
            .collect::<Vec<_>>();
        assert_eq!(
            messages.len(),
            1,
            "constructor contribution should accept the good keyword call and reject only the bad value type: {messages:#?}"
        );
        assert!(
            messages[0].contains("Argument is incorrect") || messages[0].contains("not assignable"),
            "expected an argument type diagnostic, got {messages:#?}"
        );

        Ok(())
    }

    #[test]
    fn semantic_project_index_applies_contributed_field_set_type() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/models.py",
            r#"
            class Target:
                pass

            def check(target: Target) -> None:
                good_read: int = target.contributed
                bad_read: str = target.contributed
                target.contributed = "ok"
                target.contributed = 1
            "#,
        )?;

        let plugin_id = "example.field-contribution-set-type".to_string();
        db.register_semantic_plugin_executor(plugin_id.clone(), |request| match request {
            protocol::PluginRequest::BuildProjectIndex(request) => {
                assert!(
                    request
                        .classes
                        .iter()
                        .any(|class| class.qualified_name == "models.Target"),
                    "project index should receive Target summary: {request:#?}"
                );
                Ok(protocol::PluginResponse::ProjectIndex(
                    protocol::ProjectIndexResponse {
                        plugin_index: serde_json::Value::Null,
                        contributions: vec![protocol::Contribution {
                            source: protocol::SymbolSource::default(),
                            target: protocol::ContributionTarget::Instance {
                                qualified_name: "models.Target".to_string(),
                            },
                            patch: protocol::ContributionPatch::Field(protocol::FieldPatch {
                                name: "contributed".to_string(),
                                mode: protocol::MemberPatchMode::FillOnMiss,
                                descriptor: None,
                                instance_get_type: protocol::TypeExpr::annotation("int"),
                                instance_set_type: Some(protocol::TypeExpr::annotation("str")),
                                constructor_parameter: None,
                                has_default: true,
                            }),
                            conflict_key: "models.Target.contributed".to_string(),
                            diagnostics: Vec::new(),
                        }],
                        virtual_types: Vec::new(),
                        dependencies: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                ))
            }
            _ => Ok(protocol::PluginResponse::NoChange),
        });
        install_semantic_plugin(
            &mut db,
            SemanticPlugin::new(
                plugin_id,
                SemanticPluginRuntime::InProcess,
                Vec::<String>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<String>::new(),
                Vec::<String>::new(),
            )
            .with_project_index_enabled(true),
        );

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let diagnostics = db.check_file(file);
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.primary_message())
            .collect::<Vec<_>>();
        assert!(
            messages
                .iter()
                .filter(|message| message.contains("not assignable"))
                .count()
                >= 2,
            "contributed field read type should be int and assignment type should be str: {messages:#?}"
        );

        Ok(())
    }

    #[test]
    fn semantic_project_index_receives_static_settings_summaries() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/toy.py",
            r#"
            class Model: ...
            "#,
        )?;
        db.write_dedented(
            "/src/base_settings.py",
            r#"
            SHARED_APPS = ["shared"]
            AUTH_MODEL = "accounts.User"
            DB_ENGINE = "postgresql"
            "#,
        )?;
        db.write_dedented(
            "/src/minidjango_settings.py",
            r#"
            from base_settings import SHARED_APPS, AUTH_MODEL as IMPORTED_AUTH_USER_MODEL
            import base_settings as base

            BASE_APPS = SHARED_APPS + ["library"]
            EXTRA_APPS = ["accounts"]
            INSTALLED_APPS = BASE_APPS + EXTRA_APPS
            AUTH_USER_MODEL = IMPORTED_AUTH_USER_MODEL
            MINIDJANGO_PK_TYPE = "u" + "uid"
            FEATURE_ENABLED: bool = True
            RETRY_CODES = (1,) + (2,)
            MAX_RETRIES = 1 + 2
            DATABASES = {"default": {"ENGINE": base.DB_ENGINE}}
            TIME_ZONE = build_timezone()
            helper_name = "ignored"
            "#,
        )?;
        db.write_dedented(
            "/src/models.py",
            r#"
            import toy
            from typing import Self

            def build_manager() -> object:
                return object()

            manager = build_manager()

            class Model(toy.Model):
                def active(self, flag: bool) -> Self:
                    return self
            "#,
        )?;

        let plugin_id = "example.settings-index".to_string();
        db.register_semantic_plugin_executor(plugin_id.clone(), |request| match request {
            protocol::PluginRequest::BuildProjectIndex(request) => {
                assert_eq!(
                    request.context.config["strict_settings"],
                    serde_json::json!(false)
                );
                let model = request
                    .classes
                    .iter()
                    .find(|class| class.qualified_name == "models.Model")
                    .unwrap_or_else(|| panic!("expected model summary: {request:#?}"));
                let [method] = model.methods.as_slice() else {
                    panic!("expected method summary: {model:#?}");
                };
                assert_eq!(method.name, "active");
                assert_eq!(method.parameters.len(), 2);
                assert!(
                    matches!(
                        method
                            .return_type
                            .as_ref()
                            .and_then(|ty| ty.snapshot.as_deref()),
                        Some(protocol::TypeSnapshot::SelfType { .. })
                    ),
                    "expected Self snapshot: {:#?}",
                    method.return_type
                );
                let assignment = request
                    .assignments
                    .iter()
                    .find(|assignment| assignment.qualified_name == "models.manager")
                    .unwrap_or_else(|| panic!("expected assignment summary: {request:#?}"));
                assert!(
                    matches!(
                        &assignment.assigned_value,
                        protocol::AssignedValueSummary::Call(call)
                            if call.callee.qualified_name.ends_with("build_manager")
                    ),
                    "expected build_manager call: {assignment:#?}"
                );
                let settings = request
                    .settings
                    .iter()
                    .find(|settings| settings.module == "minidjango_settings")
                    .unwrap_or_else(|| panic!("expected settings summary: {request:#?}"));
                assert_eq!(
                    settings
                        .values
                        .iter()
                        .map(|value| value.name.as_str())
                        .collect::<Vec<_>>(),
                    [
                        "BASE_APPS",
                        "EXTRA_APPS",
                        "INSTALLED_APPS",
                        "AUTH_USER_MODEL",
                        "MINIDJANGO_PK_TYPE",
                        "FEATURE_ENABLED",
                        "RETRY_CODES",
                        "MAX_RETRIES",
                        "DATABASES"
                    ]
                );
                let setting_value = |name: &str| {
                    settings
                        .values
                        .iter()
                        .find(|value| value.name == name)
                        .unwrap_or_else(|| panic!("expected setting `{name}`: {settings:#?}"))
                        .value
                        .clone()
                };
                assert_eq!(
                    setting_value("INSTALLED_APPS"),
                    protocol::LiteralValue::List {
                        items: vec![
                            protocol::LiteralValue::Str {
                                value: "shared".to_string()
                            },
                            protocol::LiteralValue::Str {
                                value: "library".to_string()
                            },
                            protocol::LiteralValue::Str {
                                value: "accounts".to_string()
                            }
                        ]
                    }
                );
                assert_eq!(
                    setting_value("AUTH_USER_MODEL"),
                    protocol::LiteralValue::Str {
                        value: "accounts.User".to_string()
                    }
                );
                assert_eq!(
                    setting_value("MINIDJANGO_PK_TYPE"),
                    protocol::LiteralValue::Str {
                        value: "uuid".to_string()
                    }
                );
                assert_eq!(
                    setting_value("FEATURE_ENABLED"),
                    protocol::LiteralValue::Bool { value: true }
                );
                assert_eq!(
                    setting_value("RETRY_CODES"),
                    protocol::LiteralValue::Tuple {
                        items: vec![
                            protocol::LiteralValue::Int { value: 1 },
                            protocol::LiteralValue::Int { value: 2 }
                        ]
                    }
                );
                assert_eq!(
                    setting_value("MAX_RETRIES"),
                    protocol::LiteralValue::Int { value: 3 }
                );
                assert_eq!(
                    setting_value("DATABASES"),
                    protocol::LiteralValue::Dict {
                        entries: vec![protocol::LiteralDictEntry {
                            key: protocol::LiteralValue::Str {
                                value: "default".to_string()
                            },
                            value: protocol::LiteralValue::Dict {
                                entries: vec![protocol::LiteralDictEntry {
                                    key: protocol::LiteralValue::Str {
                                        value: "ENGINE".to_string()
                                    },
                                    value: protocol::LiteralValue::Str {
                                        value: "postgresql".to_string()
                                    }
                                }]
                            }
                        }]
                    }
                );
                assert_eq!(settings.dependencies.len(), 2);
                assert!(
                    settings
                        .dependencies
                        .iter()
                        .any(|dependency| dependency.path.ends_with("/src/base_settings.py"))
                );
                assert!(
                    settings
                        .dependencies
                        .iter()
                        .any(|dependency| dependency.path.ends_with("/src/minidjango_settings.py"))
                );
                let [unsupported] = settings.diagnostics.as_slice() else {
                    panic!("expected unsupported setting diagnostic: {settings:#?}");
                };
                assert_eq!(unsupported.id, "ty.settings.unsupported-value");
                assert_eq!(unsupported.severity, protocol::DiagnosticSeverity::Warning);
                assert!(unsupported.location.is_some());

                let missing = request
                    .settings
                    .iter()
                    .find(|settings| settings.module == "missing_settings")
                    .unwrap_or_else(|| panic!("expected missing settings summary: {request:#?}"));
                let [missing_diagnostic] = missing.diagnostics.as_slice() else {
                    panic!("expected missing settings diagnostic: {missing:#?}");
                };
                assert_eq!(missing_diagnostic.id, "ty.settings.module-not-found");
                assert_eq!(
                    missing_diagnostic.severity,
                    protocol::DiagnosticSeverity::Warning
                );

                Ok(protocol::PluginResponse::ProjectIndex(
                    protocol::ProjectIndexResponse {
                        plugin_index: serde_json::json!({ "pk_type": "uuid" }),
                        contributions: Vec::new(),
                        virtual_types: Vec::new(),
                        dependencies: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                ))
            }
            protocol::PluginRequest::AnalyzeClass(request) => {
                assert_eq!(
                    request
                        .project_index
                        .as_ref()
                        .and_then(|index| index.get("pk_type")),
                    Some(&serde_json::json!("uuid"))
                );
                Ok(protocol::PluginResponse::ClassPatch(protocol::ClassPatch {
                    fields: Vec::new(),
                    class_members: Vec::new(),
                    instance_members: Vec::new(),
                    constructor: None,
                    diagnostics: Vec::new(),
                }))
            }
            _ => Ok(protocol::PluginResponse::NoChange),
        });
        install_semantic_plugin(
            &mut db,
            SemanticPlugin::new(
                plugin_id,
                SemanticPluginRuntime::InProcess,
                vec!["toy.Model".to_string()],
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<String>::new(),
                Vec::<String>::new(),
            )
            .with_settings_module_claims(vec![
                "minidjango_settings".to_string(),
                "missing_settings".to_string(),
            ])
            .with_project_index_enabled(true),
        );

        let class = static_class_literal(&db, "/src/models.py", "Model");
        let patch = class.plugin_class_transform_patch(&db);
        assert!(patch.fields.is_empty());

        let settings_file =
            system_path_to_file(&db, "/src/minidjango_settings.py").expect("settings file");
        let diagnostics = db.check_file(settings_file);
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic
                    .primary_message()
                    .contains("is not a supported static literal")
            })
            .unwrap_or_else(|| panic!("expected settings diagnostic: {diagnostics:#?}"));
        assert_eq!(diagnostic.severity(), Severity::Warning);

        Ok(())
    }

    #[test]
    fn semantic_plugin_validates_subclass_item_mutations() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/models.py",
            r#"
            class Immutable:
                def __setitem__(self, key: str, value: str) -> None: ...

            class Child(Immutable): ...

            value = Child()
            value["name"] = "Ada"
            "#,
        )?;

        let plugin_id = "example.mutation".to_string();
        db.register_semantic_plugin_executor(plugin_id.clone(), |request| {
            let protocol::PluginRequest::ValidateMutation(request) = request else {
                return Ok(protocol::PluginResponse::NoChange);
            };
            assert_eq!(request.operation, protocol::MutationOperation::ItemSet);
            assert_eq!(request.receiver.expression, "models.Child");
            assert!(matches!(
                request.key.as_ref().map(|key| &key.value),
                Some(protocol::LiteralValue::Str { value }) if value == "name"
            ));
            assert!(matches!(
                request.value.as_ref().map(|value| &value.value),
                Some(protocol::LiteralValue::Str { value }) if value == "Ada"
            ));
            assert_eq!(request.source.file_path.as_deref(), Some("/src/models.py"));

            Ok(protocol::PluginResponse::MutationDiagnostics(
                protocol::MutationResponse {
                    diagnostics: vec![protocol::PluginDiagnostic {
                        id: "example.immutable-write".to_string(),
                        message: "immutable item write".to_string(),
                        severity: protocol::DiagnosticSeverity::Error,
                        location: None,
                        metadata: Default::default(),
                    }],
                },
            ))
        });
        install_semantic_plugin(
            &mut db,
            SemanticPlugin::new(
                plugin_id,
                SemanticPluginRuntime::InProcess,
                Vec::<String>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<String>::new(),
                Vec::<String>::new(),
            )
            .with_mutation_claims(Vec::<String>::new(), vec!["models.Immutable".to_string()]),
        );

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let diagnostics = db.check_file(file);
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.primary_message() == "immutable item write"),
            "expected plugin mutation diagnostic: {diagnostics:#?}"
        );

        Ok(())
    }

    #[test]
    fn semantic_plugin_call_summary_preserves_named_boolean_keywords() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/models.py",
            r#"
            class Manager:
                def values_list(self, *fields: str, named: bool = False) -> object:
                    return object()

            manager = Manager()
            result = manager.values_list("title", "pages", named=True)
            "#,
        )?;

        let called = Arc::new(AtomicUsize::new(0));
        let called_for_executor = Arc::clone(&called);
        let plugin_id = "example.call-summary".to_string();
        db.register_semantic_plugin_executor(plugin_id.clone(), move |request| {
            let protocol::PluginRequest::AdjustCallReturn(request) = request else {
                return Ok(protocol::PluginResponse::NoChange);
            };
            called_for_executor.fetch_add(1, Ordering::SeqCst);
            let named = request
                .arguments
                .iter()
                .find(|argument| argument.name.as_deref() == Some("named"))
                .unwrap_or_else(|| panic!("expected named keyword: {request:#?}"));
            assert_eq!(named.kind, protocol::ArgumentKind::Keyword);
            assert_eq!(named.value, protocol::LiteralValue::Bool { value: true });
            Ok(protocol::PluginResponse::NoChange)
        });
        install_semantic_plugin(
            &mut db,
            SemanticPlugin::new(
                plugin_id,
                SemanticPluginRuntime::InProcess,
                Vec::<String>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<String>::new(),
                vec!["models.Manager.values_list".to_string()],
            ),
        );

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        db.check_file(file);
        assert_eq!(called.load(Ordering::SeqCst), 1);

        Ok(())
    }

    #[test]
    fn annotated_cast_preserves_metadata_on_the_result() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/models.py",
            r#"
            from typing import Annotated, cast

            class Model: ...
            class Metadata: ...

            annotated = cast(Annotated[Model, Metadata], Model())
            "#,
        )?;

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let annotated = global_symbol(&db, file, "annotated").place.expect_type();
        let Type::KnownInstance(KnownInstanceType::Annotated(annotated)) = annotated else {
            panic!("expected an Annotated cast result, got {annotated:?}");
        };
        assert_eq!(annotated.base(&db).display(&db).to_string(), "Model");
        assert_eq!(annotated.metadata(&db).len(), 1);
        assert!(
            db.check_file(file)
                .iter()
                .all(|diagnostic| diagnostic.id().as_str() != "redundant-cast")
        );

        Ok(())
    }

    #[test]
    fn semantic_project_index_strict_settings_promotes_settings_diagnostics() -> anyhow::Result<()>
    {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/minidjango_settings.py",
            r#"
            INSTALLED_APPS = ["library"]
            TIME_ZONE = build_timezone()
            "#,
        )?;

        let plugin_id = "example.strict-settings-index".to_string();
        db.register_semantic_plugin_executor(plugin_id.clone(), |request| match request {
            protocol::PluginRequest::BuildProjectIndex(request) => {
                assert_eq!(
                    request.context.config["strict_settings"],
                    serde_json::json!(true)
                );

                let settings = request
                    .settings
                    .iter()
                    .find(|settings| settings.module == "minidjango_settings")
                    .unwrap_or_else(|| panic!("expected settings summary: {request:#?}"));
                let [unsupported] = settings.diagnostics.as_slice() else {
                    panic!("expected unsupported setting diagnostic: {settings:#?}");
                };
                assert_eq!(unsupported.id, "ty.settings.unsupported-value");
                assert_eq!(unsupported.severity, protocol::DiagnosticSeverity::Error);

                let missing = request
                    .settings
                    .iter()
                    .find(|settings| settings.module == "missing_settings")
                    .unwrap_or_else(|| panic!("expected missing settings summary: {request:#?}"));
                let [missing_diagnostic] = missing.diagnostics.as_slice() else {
                    panic!("expected missing settings diagnostic: {missing:#?}");
                };
                assert_eq!(missing_diagnostic.id, "ty.settings.module-not-found");
                assert_eq!(
                    missing_diagnostic.severity,
                    protocol::DiagnosticSeverity::Error
                );

                Ok(protocol::PluginResponse::ProjectIndex(
                    protocol::ProjectIndexResponse {
                        plugin_index: serde_json::Value::Null,
                        contributions: Vec::new(),
                        virtual_types: Vec::new(),
                        dependencies: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                ))
            }
            _ => Ok(protocol::PluginResponse::NoChange),
        });
        install_semantic_plugin(
            &mut db,
            SemanticPlugin::new(
                plugin_id,
                SemanticPluginRuntime::InProcess,
                Vec::<String>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<String>::new(),
                Vec::<String>::new(),
            )
            .with_settings_module_claims(vec![
                "minidjango_settings".to_string(),
                "missing_settings".to_string(),
            ])
            .with_strict_settings(true)
            .with_project_index_enabled(true),
        );

        let settings_file =
            system_path_to_file(&db, "/src/minidjango_settings.py").expect("settings file");
        let diagnostics = db.check_file(settings_file);
        let diagnostic = diagnostics
            .iter()
            .find(|diagnostic| {
                diagnostic
                    .primary_message()
                    .contains("is not a supported static literal")
            })
            .unwrap_or_else(|| panic!("expected settings diagnostic: {diagnostics:#?}"));
        assert_eq!(diagnostic.severity(), Severity::Error);

        Ok(())
    }

    #[test]
    fn class_transform_preserves_distinct_field_get_set_and_constructor_types() -> anyhow::Result<()>
    {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/models.py",
            r#"
            class Model: ...
            "#,
        )?;
        let class = static_class_literal(&db, "/src/models.py", "Model");
        let response = protocol::PluginResponse::ClassPatch(protocol::ClassPatch {
            fields: vec![protocol::FieldPatch {
                name: "field".to_string(),
                mode: protocol::MemberPatchMode::FillOnMiss,
                descriptor: Some(protocol::MemberAccessPatch::Descriptor {
                    class_type: Some(protocol::TypeExpr::annotation("object")),
                    instance_get_type: protocol::TypeExpr::annotation("int"),
                    instance_set_type: Some(protocol::TypeExpr::annotation("str")),
                }),
                instance_get_type: protocol::TypeExpr::annotation("int"),
                instance_set_type: Some(protocol::TypeExpr::annotation("str")),
                constructor_parameter: Some(protocol::Parameter {
                    name: Some("field".to_string()),
                    kind: protocol::ParameterKind::KeywordOnly,
                    type_expr: None,
                    required: true,
                }),
                has_default: false,
            }],
            class_members: vec![protocol::MemberPatch {
                name: "self_class_member".to_string(),
                mode: protocol::MemberPatchMode::FillOnMiss,
                access: protocol::MemberAccessPatch::value(protocol::TypeExpr::annotation("Self")),
                read_only: false,
                diagnostics: Vec::new(),
            }],
            instance_members: vec![protocol::MemberPatch {
                name: "self_instance_member".to_string(),
                mode: protocol::MemberPatchMode::FillOnMiss,
                access: protocol::MemberAccessPatch::value(protocol::TypeExpr::annotation(
                    "list[Self]",
                )),
                read_only: false,
                diagnostics: Vec::new(),
            }],
            constructor: Some(protocol::CallableSignature {
                parameters: vec![protocol::Parameter {
                    name: Some("self_value".to_string()),
                    kind: protocol::ParameterKind::KeywordOnly,
                    type_expr: Some(protocol::TypeExpr::annotation("Self")),
                    required: false,
                }],
                return_type: protocol::TypeExpr::annotation("None"),
            }),
            diagnostics: Vec::new(),
        });
        let mut fields = Vec::new();
        let mut class_members = Vec::new();
        let mut instance_members = Vec::new();
        let mut constructor = None;

        merge_plugin_class_response(
            &db,
            class,
            response,
            &[],
            &mut fields,
            &mut class_members,
            &mut instance_members,
            &mut constructor,
        );

        let [field] = fields.as_slice() else {
            panic!("expected one field patch");
        };
        assert_eq!(
            field
                .descriptor_class_ty
                .expect("class descriptor type")
                .display(&db)
                .to_string(),
            "object"
        );
        assert_eq!(field.instance_get_ty.display(&db).to_string(), "int");
        assert_eq!(
            field
                .instance_set_ty
                .expect("set type")
                .display(&db)
                .to_string(),
            "str"
        );
        let parameter = plugin_field_constructor_parameter(field).expect("constructor parameter");
        assert_eq!(parameter.annotated_type().display(&db).to_string(), "str");

        let [class_member] = class_members.as_slice() else {
            panic!("expected one class member");
        };
        assert_eq!(class_member.ty.display(&db).to_string(), "Model");
        let [instance_member] = instance_members.as_slice() else {
            panic!("expected one instance member");
        };
        assert_eq!(instance_member.ty.display(&db).to_string(), "list[Model]");
        let constructor = constructor.as_ref().expect("constructor patch");
        let [constructor_parameter] = constructor.parameters.as_ref() else {
            panic!("expected one constructor parameter");
        };
        assert_eq!(constructor_parameter.ty.display(&db).to_string(), "Model");

        Ok(())
    }

    #[test]
    fn plugin_field_assignment_uses_instance_set_type() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/models.py",
            r#"
            class Base: ...

            def field_factory() -> int:
                return 1

            class Model(Base):
                value = field_factory()

            class Child(Model): ...

            def check(model: Model, child: Child) -> None:
                model.value = "ok"
                model.value = 1
                child.value = "child"
                child.value = 2
            "#,
        )?;

        db.register_semantic_plugin_executor("example.assignment".to_string(), |request| {
            let protocol::PluginRequest::AnalyzeClass(request) = request else {
                return Ok(protocol::PluginResponse::NoChange);
            };
            if request.class.qualified_name != "models.Model" {
                return Ok(protocol::PluginResponse::NoChange);
            }

            Ok(protocol::PluginResponse::ClassPatch(protocol::ClassPatch {
                fields: vec![
                    protocol::FieldPatch {
                        name: "value".to_string(),
                        mode: protocol::MemberPatchMode::FillOnMiss,
                        descriptor: Some(protocol::MemberAccessPatch::Descriptor {
                            class_type: Some(protocol::TypeExpr::annotation("int")),
                            instance_get_type: protocol::TypeExpr::annotation("int"),
                            instance_set_type: Some(protocol::TypeExpr::annotation("str")),
                        }),
                        instance_get_type: protocol::TypeExpr::annotation("int"),
                        instance_set_type: Some(protocol::TypeExpr::annotation("str")),
                        constructor_parameter: None,
                        has_default: true,
                    },
                    protocol::FieldPatch {
                        name: "virtual".to_string(),
                        mode: protocol::MemberPatchMode::FillOnMiss,
                        descriptor: Some(protocol::MemberAccessPatch::Descriptor {
                            class_type: Some(protocol::TypeExpr::annotation("object")),
                            instance_get_type: protocol::TypeExpr::annotation("int"),
                            instance_set_type: Some(protocol::TypeExpr::annotation("str")),
                        }),
                        instance_get_type: protocol::TypeExpr::annotation("int"),
                        instance_set_type: Some(protocol::TypeExpr::annotation("str")),
                        constructor_parameter: None,
                        has_default: true,
                    },
                ],
                class_members: Vec::new(),
                instance_members: Vec::new(),
                constructor: None,
                diagnostics: Vec::new(),
            }))
        });
        install_semantic_plugin(
            &mut db,
            SemanticPlugin::new(
                "example.assignment".to_string(),
                SemanticPluginRuntime::InProcess,
                vec!["models.Base".to_string()],
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<String>::new(),
                Vec::<String>::new(),
            ),
        );

        let class = static_class_literal(&db, "/src/models.py", "Model");
        let field = class
            .plugin_class_transform_patch(&db)
            .fields
            .iter()
            .find(|field| field.name.as_str() == "value")
            .expect("plugin field");
        assert_eq!(field.instance_get_ty.display(&db).to_string(), "int");
        assert_eq!(
            field
                .instance_set_ty
                .expect("set type")
                .display(&db)
                .to_string(),
            "str"
        );
        assert_eq!(
            class
                .class_member(&db, "virtual", MemberLookupPolicy::default())
                .place
                .raw_type()
                .expect("virtual class descriptor")
                .display(&db)
                .to_string(),
            "object"
        );
        let child = static_class_literal(&db, "/src/models.py", "Child");
        assert_eq!(
            child
                .class_member(&db, "virtual", MemberLookupPolicy::default())
                .place
                .raw_type()
                .expect("inherited virtual class descriptor")
                .display(&db)
                .to_string(),
            "object"
        );

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let diagnostics = db.check_file(file);
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.primary_message())
            .collect::<Vec<_>>();
        assert_eq!(
            messages.len(),
            2,
            "string assignments should pass and int assignments should fail on the base and subclass: {messages:#?}"
        );
        assert!(
            messages
                .iter()
                .all(|message| message.contains("not assignable")),
            "expected an assignment diagnostic, got {messages:#?}"
        );

        Ok(())
    }

    #[test]
    fn minidjango_plugin_transforms_real_class_summary_through_semantic_hooks() -> anyhow::Result<()>
    {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/minidjango.py",
            r#"
            from typing import Generic, TypeVar

            T = TypeVar("T")
            Row = TypeVar("Row")

            class Model: ...
            class QuerySet(Generic[T, Row]):
                def filter(self, **kwargs): ...
                def get(self, **kwargs): ...
                def get_or_create(self, **kwargs): ...
                def first(self): ...
                def count(self): ...
                def exists(self): ...
                def values(self, *fields): ...
                def values_list(self, *fields, flat: bool = False, named: bool = False): ...
                def annotate(self, **kwargs): ...
            class Manager(Generic[T]):
                def filter(self, **kwargs): ...
                def get(self, **kwargs): ...
                def get_or_create(self, **kwargs): ...
                def first(self): ...
                def count(self): ...
                def exists(self): ...
                def values(self, *fields): ...
                def values_list(self, *fields, flat: bool = False, named: bool = False): ...
                def annotate(self, **kwargs): ...
            def CharField(*, max_length: int, null: bool = False): ...
            def IntegerField(*, null: bool = False): ...
            def ForeignKey(to, *, null: bool = False, related_name = None): ...
            "#,
        )?;
        db.write_dedented(
            "/src/library.py",
            r#"
            import minidjango

            class Author(minidjango.Model):
                name = minidjango.CharField(max_length=100)
            "#,
        )?;
        db.write_dedented(
            "/src/accounts.py",
            r#"
            import minidjango

            class User(minidjango.Model):
                username = minidjango.CharField(max_length=100)
            "#,
        )?;
        db.write_dedented(
            "/src/minidjango_settings.py",
            r#"
            AUTH_USER_MODEL = "accounts.User"
            "#,
        )?;
        db.write_dedented(
            "/src/models.py",
            r#"
            from typing import TypedDict

            import minidjango
            import library
            import accounts
            import minidjango_settings

            class BookManager: ...

            class BookValueRow(TypedDict):
                title: str
                pages: int | None

            class BookTitleRow(TypedDict):
                title: str

            class Book(minidjango.Model):
                title = minidjango.CharField(max_length=200)
                pages = minidjango.IntegerField(null=True)
                author = minidjango.ForeignKey("library.Author", related_name="books")
                alternate_author = minidjango.ForeignKey("library.Author", null=True, related_name="books")
                parent = minidjango.ForeignKey("self", null=True, related_name="children")
                missing_author = minidjango.ForeignKey("library.Missing", null=True, related_name="missing_books")
                owner = minidjango.ForeignKey(minidjango_settings.AUTH_USER_MODEL, null=True, related_name="owned_books")
                published = BookManager()

            def check(a: library.Author, b: Book, u: accounts.User) -> None:
                Book(title="ok", author=a)
                Book(title=123, author=a)
                Book(title="ok", author=b)
                default_manager: minidjango.Manager[Book] = Book._default_manager
                bad_default_manager: minidjango.Manager[library.Author] = Book._default_manager
                custom_manager: minidjango.Manager[Book] = Book.published
                bad_custom_manager: minidjango.Manager[library.Author] = Book.published
                books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title="ok")
                bad_books: minidjango.QuerySet[library.Author, library.Author] = Book.objects.filter(title="ok")
                published_books: minidjango.QuerySet[Book, Book] = Book.published.filter(title="ok")
                bad_published_books: minidjango.QuerySet[library.Author, library.Author] = Book.published.filter(title="ok")
                chained_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title="ok").filter(pages=1)
                bad_chained_books: minidjango.QuerySet[library.Author, library.Author] = Book.objects.filter(title="ok").filter(pages=1)
                exact_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title__exact="ok")
                iexact_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title__iexact="ok")
                contains_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title__contains="ok")
                regex_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title__regex="^ok")
                iregex_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(title__iregex="^ok")
                nullable_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(pages__isnull=True)
                range_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(pages__range=(1, 10))
                author_named_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(author__name="Ada")
                owner_named_books: minidjango.QuerySet[Book, Book] = Book.objects.filter(owner__username="kev")
                Book.objects.filter(missing="bad")
                Book.objects.get(title__year="bad")
                Book.objects.filter(author__missing="bad")
                value_rows: minidjango.QuerySet[Book, BookValueRow] = Book.objects.values("title", "pages")
                bad_value_rows: minidjango.QuerySet[Book, dict[str, int]] = Book.objects.values("title", "pages")
                chained_value_rows: minidjango.QuerySet[Book, BookTitleRow] = Book.objects.values("title").filter(author__name="Ada")
                bad_chained_value_rows: minidjango.QuerySet[Book, dict[str, int]] = Book.objects.values("title").filter(author__name="Ada")
                Book.objects.values("missing")
                title_rows: minidjango.QuerySet[Book, str] = Book.objects.values_list("title", flat=True)
                bad_title_rows: minidjango.QuerySet[Book, int] = Book.objects.values_list("title", flat=True)
                page_rows: minidjango.QuerySet[Book, int | None] = Book.objects.values_list("pages", flat=True)
                bad_page_rows: minidjango.QuerySet[Book, str] = Book.objects.values_list("pages", flat=True)
                title_page_rows: minidjango.QuerySet[Book, tuple[str, int | None]] = Book.objects.values_list("title", "pages")
                bad_title_page_rows: minidjango.QuerySet[Book, tuple[int, str]] = Book.objects.values_list("title", "pages")
                chained_title_rows: minidjango.QuerySet[Book, str] = Book.objects.values_list("title", flat=True).filter(pages=1)
                bad_chained_title_rows: minidjango.QuerySet[Book, int] = Book.objects.values_list("title", flat=True).filter(pages=1)
                chained_title_page_rows: minidjango.QuerySet[Book, tuple[str, int | None]] = Book.objects.values_list("title", "pages").filter(title="ok")
                bad_chained_title_page_rows: minidjango.QuerySet[Book, tuple[int, str]] = Book.objects.values_list("title", "pages").filter(title="ok")
                named_title_value: str = Book.objects.values_list("title", named=True).get().title
                bad_named_title_value: int = Book.objects.values_list("title", named=True).get().title
                chained_named_page_value: int | None = Book.objects.values_list("title", "pages", named=True).filter(title="ok").get().pages
                bad_chained_named_page_value: str = Book.objects.values_list("title", "pages", named=True).filter(title="ok").get().pages
                Book.objects.values_list("missing", flat=True)
                book: Book = Book.objects.get(title="ok")
                bad_author: library.Author = Book.objects.get(title="ok")
                created: tuple[Book, bool] = Book.objects.get_or_create(title="ok")
                bad_created: tuple[library.Author, bool] = Book.objects.get_or_create(title="ok")
                default_book: Book = Book._default_manager.get(title="ok")
                bad_default_author: library.Author = Book._default_manager.get(title="ok")
                queryset_book: Book = Book.objects.filter(title="ok").get(pages=1)
                bad_queryset_author: library.Author = Book.objects.filter(title="ok").get(pages=1)
                queryset_created: tuple[Book, bool] = Book.objects.filter(title="ok").get_or_create(pages=1)
                bad_queryset_created: tuple[library.Author, bool] = Book.objects.filter(title="ok").get_or_create(pages=1)
                maybe_book: Book | None = Book.objects.first()
                title_value: str = Book.objects.values_list("title", flat=True).get()
                bad_title_value: int = Book.objects.values_list("title", flat=True).get()
                maybe_title: str | None = Book.objects.values_list("title", flat=True).first()
                bad_maybe_title: int = Book.objects.values_list("title", flat=True).first()
                title_page_value: tuple[str, int | None] = Book.objects.values_list("title", "pages").get()
                bad_title_page_value: tuple[str, int] = Book.objects.values_list("title", "pages").get()
                maybe_title_page: tuple[str, int | None] | None = Book.objects.values_list("title", "pages").first()
                bad_maybe_title_page: tuple[str, int] = Book.objects.values_list("title", "pages").first()
                value_row: BookTitleRow = Book.objects.values("title").get()
                bad_value_row: dict[str, int] = Book.objects.values("title").get()
                maybe_value_row: BookTitleRow | None = Book.objects.values("title").first()
                bad_maybe_value_row: dict[str, int] = Book.objects.values("title").first()
                all_value_title: str = Book.objects.values().get()["title"]
                bad_all_value_title: int = Book.objects.values().get()["title"]
                all_named_title: str = Book.objects.values_list(named=True).get().title
                bad_all_named_title: int = Book.objects.values_list(named=True).get().title
                queryset_count: int = Book.objects.filter(title="ok").count()
                bad_queryset_count: str = Book.objects.filter(title="ok").count()
                queryset_exists: bool = Book.objects.filter(title="ok").exists()
                bad_queryset_exists: str = Book.objects.filter(title="ok").exists()
                annotated_book: Book = Book.objects.annotate(score=1).get()
                bad_annotated_author: library.Author = Book.objects.annotate(score=1).get()
                annotated_score: int = Book.objects.annotate(score=1).get().score
                bad_annotated_score: str = Book.objects.annotate(score=1).get().score
                chained_annotated_score: int = Book.objects.filter(title="ok").annotate(score=1).filter(pages=1).get().score
                bad_chained_annotated_score: str = Book.objects.filter(title="ok").annotate(score=1).filter(pages=1).get().score
                books_from_author: minidjango.Manager[Book] = a.books
                bad_reverse: minidjango.Manager[library.Author] = a.books
                children: minidjango.Manager[Book] = b.children
                bad_children: minidjango.Manager[library.Author] = b.children
                owned_books: minidjango.Manager[Book] = u.owned_books
                bad_owned_books: minidjango.Manager[library.Author] = u.owned_books
            "#,
        )?;

        let sdk_plugin = MiniDjangoPlugin;
        let plugin_id = sdk_plugin.manifest().id;
        db.register_semantic_plugin_executor(plugin_id.clone(), move |request| {
            Ok(sdk_plugin.handle(request))
        });
        install_semantic_plugin(
            &mut db,
            SemanticPlugin::new(
                plugin_id,
                SemanticPluginRuntime::InProcess,
                vec![minidjango::MODEL_BASE.to_string()],
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<SemanticPluginMemberClaim>::new(),
                Vec::<String>::new(),
                Vec::<String>::new(),
            )
            .with_call_method_on_subclass_claims(
                Vec::<SemanticPluginMethodClaim>::new(),
                vec![
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "filter"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "get"),
                    SemanticPluginMethodClaim::on_subclass_of(
                        minidjango::MANAGER_BASE,
                        "get_or_create",
                    ),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "first"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "count"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "exists"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "values"),
                    SemanticPluginMethodClaim::on_subclass_of(
                        minidjango::MANAGER_BASE,
                        "values_list",
                    ),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::MANAGER_BASE, "annotate"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "filter"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "get"),
                    SemanticPluginMethodClaim::on_subclass_of(
                        minidjango::QUERYSET_BASE,
                        "get_or_create",
                    ),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "first"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "count"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "exists"),
                    SemanticPluginMethodClaim::on_subclass_of(minidjango::QUERYSET_BASE, "values"),
                    SemanticPluginMethodClaim::on_subclass_of(
                        minidjango::QUERYSET_BASE,
                        "values_list",
                    ),
                    SemanticPluginMethodClaim::on_subclass_of(
                        minidjango::QUERYSET_BASE,
                        "annotate",
                    ),
                ],
            )
            .with_settings_module_claims(vec!["minidjango_settings".to_string()])
            .with_project_index_enabled(true),
        );

        let class = static_class_literal(&db, "/src/models.py", "Book");
        let patch = class.plugin_class_transform_patch(&db);

        let field_type = |name: &str| {
            patch
                .fields
                .iter()
                .find(|field| field.name.as_str() == name)
                .unwrap_or_else(|| panic!("expected field `{name}`"))
                .instance_get_ty
                .display(&db)
                .to_string()
        };

        assert_eq!(field_type("id"), "int");
        assert_eq!(field_type("pk"), "int");
        assert_eq!(field_type("title"), "str");
        assert_eq!(field_type("pages"), "int | None");
        assert_eq!(field_type("author"), "Author");
        assert_eq!(field_type("author_id"), "int");
        assert_eq!(field_type("parent"), "Book | None");
        assert_eq!(field_type("parent_id"), "int | None");
        assert_eq!(field_type("owner"), "User | None");
        assert_eq!(field_type("owner_id"), "int | None");

        let manager_type = |name: &str| {
            patch
                .class_members
                .iter()
                .find(|member| member.name.as_str() == name)
                .unwrap_or_else(|| panic!("expected `{name}` manager"))
                .ty
                .display(&db)
                .to_string()
        };
        assert_eq!(manager_type("objects"), "Manager");
        assert_eq!(manager_type("_default_manager"), "Manager");
        assert_eq!(manager_type("published"), "Manager");

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let diagnostics = db.check_file(file);
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.primary_message())
            .collect::<Vec<_>>();
        assert!(
            messages
                .iter()
                .any(|message| *message == "Argument is incorrect"),
            "the valid plugin constructor call should pass and the bad title type should fail: {messages:#?}"
        );
        assert!(
            messages.len() >= 3,
            "constructor, manager return hooks, and reverse relation contributions should produce focused failures: {messages:#?}"
        );
        assert!(
            messages
                .iter()
                .filter(|message| message.contains("not assignable"))
                .count()
                >= 14,
            "manager receiver return hooks and reverse relation contributions should make bad assignments fail: {messages:#?}"
        );
        assert!(
            messages
                .iter()
                .filter(|message| message.contains("Unknown Mini-Django lookup"))
                .count()
                >= 3,
            "bad direct, unsupported transformed, and values_list lookups should produce plugin diagnostics: {messages:#?}"
        );
        assert!(
            messages.iter().any(|message| *message
                == "Conflicting Mini-Django reverse relation `library.Author.books`"),
            "duplicate reverse relation names should produce a project-index diagnostic: {messages:#?}"
        );
        assert!(
            messages.iter().any(|message| *message
                == "Unknown Mini-Django relation target `library.Missing` for field `models.Book.missing_author`"),
            "bad relation targets should produce a project-index diagnostic at the field source: {messages:#?}"
        );
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("title__exact")),
            "the supported field__exact lookup should not produce a plugin diagnostic: {messages:#?}"
        );
        let unknown_lookup_messages = messages
            .iter()
            .filter(|message| message.contains("Unknown Mini-Django lookup"))
            .collect::<Vec<_>>();
        assert!(
            !unknown_lookup_messages.iter().any(|message| {
                message.contains("title__contains")
                    || message.contains("pages__isnull")
                    || message.contains("author__name")
                    || message.contains("owner__username")
            }),
            "supported terminal and relation lookups should not produce plugin diagnostics: {messages:#?}"
        );
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("No parameter named")),
            "valid plugin constructor keywords should not be rejected: {messages:#?}"
        );

        Ok(())
    }

    #[test]
    fn minidjango_fixture_harness_runs_through_semantic_hooks() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        write_minidjango_harness(&mut db)?;
        install_minidjango_plugin(&mut db);
        let annotate_returns = Arc::new(Mutex::new(Vec::new()));
        {
            let annotate_returns = Arc::clone(&annotate_returns);
            let sdk_plugin = MiniDjangoPlugin;
            let plugin_id = sdk_plugin.manifest().id;
            db.register_semantic_plugin_executor(plugin_id, move |request| {
                let response = sdk_plugin.handle(request);
                if let protocol::PluginRequest::AdjustCallReturn(call) = request
                    && call.callee.expression.ends_with(".annotate")
                {
                    let return_expression = match &response {
                        protocol::PluginResponse::CallReturnPatch(patch) => {
                            patch.return_type.expression.clone()
                        }
                        _ => "<no change>".to_string(),
                    };
                    annotate_returns.lock().unwrap().push(return_expression);
                }
                Ok(response)
            });
        }

        let class = static_class_literal(&db, "/src/models.py", "Book");
        let patch = class.plugin_class_transform_patch(&db);

        let field_type = |name: &str| {
            patch
                .fields
                .iter()
                .find(|field| field.name.as_str() == name)
                .unwrap_or_else(|| panic!("expected field `{name}`"))
                .instance_get_ty
                .display(&db)
                .to_string()
        };
        assert_eq!(field_type("title"), "str");
        assert_eq!(field_type("pages"), "int | None");
        assert_eq!(field_type("owner"), "User | None");
        assert_eq!(field_type("owner_id"), "int | None");

        let manager_type = |name: &str| {
            patch
                .class_members
                .iter()
                .find(|member| member.name.as_str() == name)
                .unwrap_or_else(|| panic!("expected `{name}` manager"))
                .ty
                .display(&db)
                .to_string()
        };
        assert_eq!(manager_type("objects"), "Manager");
        assert_eq!(manager_type("_default_manager"), "Manager");
        assert_eq!(manager_type("published"), "Manager");

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let annotate_method_ty = global_symbol(&db, file, "annotate_method_probe")
            .place
            .expect_type()
            .display(&db)
            .to_string();
        assert!(
            annotate_method_ty != "Unknown",
            "Book.objects.annotate should resolve to a callable, got {annotate_method_ty}"
        );
        let annotated_probe_ty = global_symbol(&db, file, "annotated_probe")
            .place
            .expect_type()
            .display(&db)
            .to_string();
        let observed_annotate_returns = annotate_returns.lock().unwrap().clone();
        assert_eq!(
            annotated_probe_ty, "MiniDjangoAnnotatedRow",
            "observed annotate returns: {observed_annotate_returns:#?}"
        );
        let annotated_score_ty = global_symbol(&db, file, "annotated_score_probe")
            .place
            .expect_type()
            .display(&db)
            .to_string();
        assert!(
            matches!(annotated_score_ty.as_str(), "int" | "Literal[1]"),
            "annotate score should retain the keyword argument type, got {annotated_score_ty}"
        );
        let diagnostics = db.check_file(file);
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.primary_message())
            .collect::<Vec<_>>();
        assert!(
            messages
                .iter()
                .any(|message| *message == "Object of type `str` is not assignable to `int`"),
            "Django-style plugin fields should expose their instance type, not their raw class-body field value: {messages:#?}"
        );
        assert!(
            !messages.iter().any(|message| message.contains("CharField")),
            "Django-style plugin fields should not leak raw field classes into instance access: {messages:#?}"
        );

        assert!(
            messages
                .iter()
                .any(|message| *message == "Argument is incorrect"),
            "bad constructor calls should fail while valid constructor calls pass: {messages:#?}"
        );
        assert!(
            messages
                .iter()
                .filter(|message| message.contains("not assignable"))
                .count()
                >= 24,
            "manager/queryset/values/annotate/reverse relation hooks should reject bad assignments: {messages:#?}"
        );
        assert!(
            messages
                .iter()
                .filter(|message| *message == &"Object of type `Book` is not assignable to `Author`")
                .count()
                >= 3,
            "manager and queryset hooks should reject bad model assignments: {messages:#?}"
        );
        assert!(
            messages
                .iter()
                .filter(|message| message.contains("Unknown Mini-Django lookup"))
                .count()
                >= 5,
            "invalid direct, transformed, relation, values, and values_list lookups should fail: {messages:#?}"
        );
        assert!(
            messages
                .iter()
                .filter(|message| message.contains("Invalid Mini-Django lookup value"))
                .count()
                >= 4,
            "invalid lookup literal values should fail: {messages:#?}"
        );
        assert!(
            messages.iter().any(|message| *message
                == "Conflicting Mini-Django reverse relation `library.Author.books`"),
            "duplicate reverse relation names should produce a project-index diagnostic: {messages:#?}"
        );
        assert!(
            messages.iter().any(|message| *message
                == "Unknown Mini-Django relation target `library.Missing` for field `models.Book.missing_author`"),
            "bad relation targets should produce a project-index diagnostic at the field source: {messages:#?}"
        );
        let unknown_lookup_messages = messages
            .iter()
            .filter(|message| message.contains("Unknown Mini-Django lookup"))
            .collect::<Vec<_>>();
        assert!(
            !unknown_lookup_messages.iter().any(|message| {
                message.contains("title__exact")
                    || message.contains("title__contains")
                    || message.contains("pages__isnull")
                    || message.contains("author__name")
                    || message.contains("owner__username")
            }),
            "supported terminal and relation lookups should not produce plugin diagnostics: {messages:#?}"
        );
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("No parameter named")),
            "valid plugin constructor keywords should not be rejected: {messages:#?}"
        );

        Ok(())
    }

    #[test]
    fn minidjango_project_index_is_cached_across_semantic_queries() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        write_minidjango_harness(&mut db)?;
        install_minidjango_plugin(&mut db);

        let build_index_calls = Arc::new(AtomicUsize::new(0));
        {
            let build_index_calls = Arc::clone(&build_index_calls);
            let sdk_plugin = MiniDjangoPlugin;
            let plugin_id = sdk_plugin.manifest().id;
            db.register_semantic_plugin_executor(plugin_id, move |request| {
                if matches!(request, protocol::PluginRequest::BuildProjectIndex(_)) {
                    build_index_calls.fetch_add(1, Ordering::SeqCst);
                }
                Ok(sdk_plugin.handle(request))
            });
        }

        let class = static_class_literal(&db, "/src/models.py", "Book");
        let patch = class.plugin_class_transform_patch(&db);
        assert!(
            patch
                .fields
                .iter()
                .any(|field| field.name.as_str() == "owner"),
            "settings-backed auth relation should be synthesized from the project index"
        );
        let warmed_build_index_calls = build_index_calls.load(Ordering::SeqCst);
        assert!(
            (1..=2).contains(&warmed_build_index_calls),
            "class transform should warm the Mini-Django project index with at most one recursive retry, got {warmed_build_index_calls}"
        );

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let diagnostics = db.check_file(file);
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.primary_message())
            .collect::<Vec<_>>();
        assert!(
            messages.iter().any(|message| *message
                == "Conflicting Mini-Django reverse relation `library.Author.books`"),
            "project-index diagnostics should flow through check_file: {messages:#?}"
        );
        assert!(
            messages.iter().any(|message| *message
                == "Invalid Mini-Django lookup value for `pages__isnull` on `models.Book.pages`; expected `int | None`"),
            "call-return diagnostics should consume the cached project index: {messages:#?}"
        );
        assert_eq!(
            build_index_calls.load(Ordering::SeqCst),
            warmed_build_index_calls,
            "check_file and call-return hooks should reuse the tracked Mini-Django project index"
        );

        let patch_again = class.plugin_class_transform_patch(&db);
        assert!(
            patch_again
                .class_members
                .iter()
                .any(|member| member.name.as_str() == "published"),
            "manager class-member synthesis should still be available from the cached index"
        );
        assert_eq!(
            build_index_calls.load(Ordering::SeqCst),
            warmed_build_index_calls,
            "repeated semantic queries with unchanged inputs should not rebuild the Mini-Django index"
        );

        Ok(())
    }

    #[test]
    fn minidjango_project_index_invalidates_when_model_source_changes() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        write_minidjango_harness(&mut db)?;
        db.write_dedented(
            "/src/models.py",
            r#"
            import minidjango

            class Book(minidjango.Model):
                title = minidjango.CharField(max_length=200)

            def check() -> None:
                Book.objects.filter(title__contains="ok")
            "#,
        )?;
        install_minidjango_plugin(&mut db);

        let build_index_calls = Arc::new(AtomicUsize::new(0));
        {
            let build_index_calls = Arc::clone(&build_index_calls);
            let sdk_plugin = MiniDjangoPlugin;
            let plugin_id = sdk_plugin.manifest().id;
            db.register_semantic_plugin_executor(plugin_id, move |request| {
                if matches!(request, protocol::PluginRequest::BuildProjectIndex(_)) {
                    build_index_calls.fetch_add(1, Ordering::SeqCst);
                }
                Ok(sdk_plugin.handle(request))
            });
        }

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let diagnostics = db.check_file(file);
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.primary_message())
            .collect::<Vec<_>>();
        assert!(
            !messages
                .iter()
                .any(|message| message.contains("Unknown Mini-Django lookup `title__contains`")),
            "initial model field should make title__contains valid: {messages:#?}"
        );
        let initial_build_index_calls = build_index_calls.load(Ordering::SeqCst);
        assert!(
            initial_build_index_calls > 0,
            "initial check_file should build the Mini-Django project index"
        );

        db.write_dedented(
            "/src/models.py",
            r#"
            import minidjango

            class Book(minidjango.Model):
                pages = minidjango.IntegerField()

            def check() -> None:
                Book.objects.filter(title__contains="ok")
            "#,
        )?;

        let file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let diagnostics = db.check_file(file);
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.primary_message())
            .collect::<Vec<_>>();
        assert!(
            messages.iter().any(|message| message
                == &"Unknown Mini-Django lookup `title__contains` for model `models.Book`"),
            "changed model source should invalidate lookup metadata: {messages:#?}"
        );
        assert!(
            build_index_calls.load(Ordering::SeqCst) > initial_build_index_calls,
            "changing a contributing model should rebuild the Mini-Django project index"
        );

        Ok(())
    }

    #[test]
    fn plugin_class_summary_includes_django_like_assignment_calls() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/models.py",
            r#"
            def model_decorator(**kwargs): ...

            class MetaBase: ...

            class Model: ...

            class models:
                CASCADE = "cascade"

            @model_decorator(enabled=True)
            class Book(Model, metaclass=MetaBase):
                title = CharField(max_length=100, null=True)
                author = ForeignKey("library.Author", related_name="books", on_delete=models.CASCADE)
                typed: int = 1
            "#,
        )?;

        let request = class_transform_request(&db, "/src/models.py", "Book");
        assert_eq!(request.class.qualified_name, "models.Book");
        assert_eq!(request.class.decorators.len(), 1);
        assert!(request.class.metaclass.is_some());

        let title = request
            .class
            .fields
            .iter()
            .find(|field| field.name == "title")
            .expect("title field");
        let protocol::AssignedValueSummary::Call(call) =
            title.assigned_value.as_ref().expect("title assignment")
        else {
            panic!("expected call summary for title");
        };
        assert_eq!(call.callee.qualified_name, "CharField");
        assert_eq!(call.arguments.len(), 2);
        assert_eq!(call.arguments[0].name.as_deref(), Some("max_length"));
        assert_eq!(
            call.arguments[0].value,
            protocol::LiteralValue::Int { value: 100 }
        );
        assert_eq!(call.arguments[1].name.as_deref(), Some("null"));
        assert_eq!(
            call.arguments[1].value,
            protocol::LiteralValue::Bool { value: true }
        );

        let author = request
            .class
            .fields
            .iter()
            .find(|field| field.name == "author")
            .expect("author field");
        let protocol::AssignedValueSummary::Call(call) =
            author.assigned_value.as_ref().expect("author assignment")
        else {
            panic!("expected call summary for author");
        };
        assert_eq!(call.callee.qualified_name, "ForeignKey");
        assert_eq!(
            call.arguments[0].value,
            protocol::LiteralValue::Str {
                value: "library.Author".to_string()
            }
        );
        assert_eq!(call.arguments[1].name.as_deref(), Some("related_name"));
        assert_eq!(
            call.arguments[1].value,
            protocol::LiteralValue::Str {
                value: "books".to_string()
            }
        );
        assert_eq!(call.arguments[2].name.as_deref(), Some("on_delete"));
        assert_eq!(
            call.arguments[2].value,
            protocol::LiteralValue::EnumRef(protocol::SymbolRef {
                qualified_name: "models.CASCADE".to_string()
            })
        );

        let typed = request
            .class
            .fields
            .iter()
            .find(|field| field.name == "typed")
            .expect("typed field");
        assert_eq!(
            typed.annotation.as_ref().map(|ty| ty.expression.as_str()),
            Some("int")
        );
        assert_eq!(
            typed.assigned_value,
            Some(protocol::AssignedValueSummary::Literal {
                value: protocol::LiteralValue::Int { value: 1 }
            })
        );

        Ok(())
    }

    #[test]
    fn plugin_class_summary_includes_nested_meta_constants_and_source() -> anyhow::Result<()> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/models.py",
            r#"
            class Model: ...

            class Book(Model):
                STATUS = "draft"
                flags = {"indexed": True}

                class Meta:
                    abstract = True
                    app_label = "library"
            "#,
        )?;

        let request = class_transform_request(&db, "/src/models.py", "Book");
        assert_eq!(
            request.class.source.qualified_name.as_deref(),
            Some("models.Book")
        );
        assert_eq!(request.class.source.module.as_deref(), Some("models"));
        assert!(request.class.source.start.is_some());

        let status = request
            .class
            .class_constants
            .iter()
            .find(|constant| constant.name == "STATUS")
            .expect("STATUS constant");
        assert_eq!(
            status.value,
            protocol::LiteralValue::Str {
                value: "draft".to_string()
            }
        );

        let meta = request
            .class
            .nested_classes
            .iter()
            .find(|nested| nested.name == "Meta")
            .expect("Meta class");
        assert_eq!(meta.qualified_name, "models.Book.Meta");
        let abstract_constant = meta
            .class_constants
            .iter()
            .find(|constant| constant.name == "abstract")
            .expect("abstract constant");
        assert_eq!(
            abstract_constant.value,
            protocol::LiteralValue::Bool { value: true }
        );
        let app_label = meta
            .class_constants
            .iter()
            .find(|constant| constant.name == "app_label")
            .expect("app_label constant");
        assert_eq!(
            app_label.value,
            protocol::LiteralValue::Str {
                value: "library".to_string()
            }
        );

        Ok(())
    }
}
