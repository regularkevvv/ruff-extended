//! Shared glue between `ty`'s type system and the plugin protocol, plus the Phase 5 call
//! signature and call return hooks.
//!
//! The class-transform and member hooks (Phases 3 and 4) live in
//! [`crate::types::class::static_literal`]; this module owns the call-site hooks and the small
//! protocol conversion helpers that both sets of hooks share.

use ruff_db::{
    files::File,
    source::{line_index, source_text},
};
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, ArgOrKeyword};
use ruff_python_parser::parse_expression;
use ruff_text_size::{Ranged, TextRange};
use ty_module_resolver::{ModuleName, all_modules, file_to_module, resolve_module_confident};
use ty_plugin_protocol as protocol;
use ty_python_core::global_scope;
use ty_python_core::program::{SemanticPlugin, SemanticPluginRuntime};
use ty_python_core::scope::ScopeId;

use crate::place::imported_symbol;
use crate::types::call::CallArguments;
use crate::types::class::{
    DynamicClassAnchor, DynamicClassLiteral, DynamicNamedTupleAnchor, DynamicNamedTupleLiteral,
    NamedTupleField, NamedTupleSpec, StaticClassLiteral, plugin_project_index_json,
    plugin_project_index_virtual_types,
};
use crate::types::signatures::ParametersKind;
use crate::types::tuple::{Tuple, TupleType};
use crate::types::typed_dict::{TypedDictFieldBuilder, TypedDictOpenness, TypedDictSchema};
use crate::types::{
    ClassBase, ClassLiteral, ClassType, FunctionType, KnownClass, KnownInstanceType, Parameter,
    Parameters, Signature, SubclassOfType, Type, TypedDictType, UnionType,
};
use crate::{Db, Program, SemanticPluginRuntimeError};

use super::display::qualified_name_components_from_scope;

/// Convert an internal type into a protocol [`TypeExpr`](protocol::TypeExpr).
///
/// This produces a display-form annotation; the host runtime only compares it as an opaque
/// string, so the exact spelling is not load-bearing.
pub(crate) fn plugin_type_expr_from_type<'db>(
    db: &'db dyn Db,
    ty: Type<'db>,
) -> protocol::TypeExpr {
    if let Type::ClassLiteral(class) = ty {
        return protocol::TypeExpr {
            expression: class.qualified_name(db).to_string(),
            imports: Vec::new(),
            mode: protocol::TypeExprMode::Annotation,
            snapshot: Some(Box::new(plugin_type_snapshot_from_type(db, ty))),
        };
    }

    protocol::TypeExpr {
        expression: ty.display(db).to_string(),
        imports: Vec::new(),
        mode: protocol::TypeExprMode::Annotation,
        snapshot: Some(Box::new(plugin_type_snapshot_from_type(db, ty))),
    }
}

fn plugin_type_snapshot_from_type<'db>(db: &'db dyn Db, ty: Type<'db>) -> protocol::TypeSnapshot {
    if let Some(class) = ty.nominal_class(db)
        && let ClassLiteral::DynamicNamedTuple(named_tuple) = class.class_literal(db)
        && let Some(identity) = named_tuple.plugin_virtual_identity(db)
    {
        return protocol::TypeSnapshot::PluginClass {
            identity: identity.to_string(),
        };
    }

    if let Some(tuple) = ty.tuple_instance_spec(db) {
        let (prefix, variadic, suffix) = match tuple.as_ref() {
            Tuple::Fixed(tuple) => (
                tuple
                    .elements_slice()
                    .iter()
                    .map(|element| plugin_type_snapshot_from_type(db, *element))
                    .collect(),
                None,
                Vec::new(),
            ),
            Tuple::Variable(tuple) => (
                tuple
                    .prefix_elements()
                    .iter()
                    .map(|element| plugin_type_snapshot_from_type(db, *element))
                    .collect(),
                Some(Box::new(plugin_type_snapshot_from_type(
                    db,
                    tuple.variable(),
                ))),
                tuple
                    .suffix_elements()
                    .iter()
                    .map(|element| plugin_type_snapshot_from_type(db, *element))
                    .collect(),
            ),
        };
        return protocol::TypeSnapshot::Tuple {
            prefix,
            variadic,
            suffix,
        };
    }

    match ty {
        Type::TypedDict(typed_dict) => {
            let fields = typed_dict
                .items(db)
                .iter()
                .map(|(name, field)| protocol::TypeSnapshotField {
                    name: name.to_string(),
                    type_snapshot: plugin_type_snapshot_from_type(db, field.declared_ty),
                    required: field.is_required(),
                    read_only: field.is_read_only(),
                })
                .collect();
            let (extra_items, closed) = match typed_dict.openness(db) {
                TypedDictOpenness::ImplicitlyOpen => (None, false),
                TypedDictOpenness::Closed => (None, true),
                TypedDictOpenness::Extra(extra) => (
                    Some(Box::new(protocol::TypeSnapshotField {
                        name: String::new(),
                        type_snapshot: plugin_type_snapshot_from_type(db, extra.declared_ty),
                        required: false,
                        read_only: extra.is_read_only(),
                    })),
                    false,
                ),
            };
            protocol::TypeSnapshot::TypedDict {
                fields,
                extra_items,
                closed,
            }
        }
        Type::Union(union) => protocol::TypeSnapshot::Union {
            elements: union
                .elements(db)
                .iter()
                .map(|element| plugin_type_snapshot_from_type(db, *element))
                .collect(),
        },
        Type::TypeVar(typevar) if typevar.typevar(db).is_self(db) => {
            protocol::TypeSnapshot::SelfType {
                bound: typevar
                    .typevar(db)
                    .upper_bound(db)
                    .map(|bound| Box::new(plugin_type_snapshot_from_type(db, bound))),
            }
        }
        Type::KnownInstance(KnownInstanceType::Annotated(annotated)) => {
            protocol::TypeSnapshot::Annotated {
                base: Box::new(plugin_type_snapshot_from_type(db, annotated.base(db))),
                metadata: annotated
                    .metadata(db)
                    .iter()
                    .map(|metadata| plugin_type_snapshot_metadata_from_type(db, *metadata))
                    .collect(),
            }
        }
        _ => {
            if let Some(class) = ty.nominal_class(db) {
                if let ClassLiteral::Dynamic(dynamic_class) = class.class_literal(db)
                    && let Some(identity) = dynamic_class.plugin_virtual_identity(db)
                {
                    return protocol::TypeSnapshot::PluginClass {
                        identity: identity.to_string(),
                    };
                }
                let arguments = match class {
                    ClassType::Generic(generic) => generic
                        .specialization(db)
                        .types(db)
                        .iter()
                        .map(|argument| plugin_type_snapshot_from_type(db, *argument))
                        .collect(),
                    ClassType::NonGeneric(_) => Vec::new(),
                };
                return protocol::TypeSnapshot::Nominal {
                    qualified_name: class.qualified_name(db).to_string(),
                    arguments,
                };
            }

            let fallback = protocol::TypeExpr {
                expression: ty.display(db).to_string(),
                imports: Vec::new(),
                mode: protocol::TypeExprMode::Annotation,
                snapshot: None,
            };
            protocol::TypeSnapshot::expression(&fallback)
        }
    }
}

fn plugin_type_snapshot_metadata_from_type(
    db: &dyn Db,
    ty: Type<'_>,
) -> protocol::TypeSnapshotMetadata {
    if let Type::GenericAlias(alias) = ty {
        return protocol::TypeSnapshotMetadata {
            qualified_name: ClassLiteral::Static(alias.origin(db))
                .qualified_name(db)
                .to_string(),
            arguments: alias
                .specialization(db)
                .types(db)
                .iter()
                .map(|argument| plugin_type_snapshot_from_type(db, *argument))
                .collect(),
        };
    }
    match plugin_type_snapshot_from_type(db, ty) {
        protocol::TypeSnapshot::Nominal {
            qualified_name,
            arguments,
        } => protocol::TypeSnapshotMetadata {
            qualified_name,
            arguments,
        },
        protocol::TypeSnapshot::Expression(expression) => protocol::TypeSnapshotMetadata {
            qualified_name: expression.expression,
            arguments: Vec::new(),
        },
        snapshot => protocol::TypeSnapshotMetadata {
            qualified_name: ty.display(db).to_string(),
            arguments: vec![snapshot],
        },
    }
}

/// Parse a protocol [`TypeExpr`](protocol::TypeExpr) back into an internal type.
///
/// This deliberately supports only a conservative subset of the MVP type-expression grammar
/// (fully qualified builtins). Anything outside that subset resolves to `Unknown` rather than
/// panicking, matching the protocol rule that invalid type expressions must not crash the host.
pub(crate) fn plugin_type_expr_to_type<'db>(
    db: &'db dyn Db,
    type_expr: &protocol::TypeExpr,
) -> Type<'db> {
    plugin_type_expr_to_type_with_context(db, type_expr, PluginTypeExprContext::default())
}

pub(crate) fn plugin_type_expr_to_type_with_virtual_types<'db>(
    db: &'db dyn Db,
    type_expr: &protocol::TypeExpr,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> Type<'db> {
    plugin_type_expr_to_type_with_context(
        db,
        type_expr,
        PluginTypeExprContext {
            virtual_types,
            ..PluginTypeExprContext::default()
        },
    )
}

pub(crate) fn plugin_type_expr_to_type_in_class_with_virtual_types<'db>(
    db: &'db dyn Db,
    type_expr: &protocol::TypeExpr,
    class: StaticClassLiteral<'db>,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> Type<'db> {
    plugin_type_expr_to_type_with_context(
        db,
        type_expr,
        PluginTypeExprContext {
            self_class: Some(class),
            scope: Some(global_scope(db, class.file(db))),
            virtual_types,
            ..PluginTypeExprContext::default()
        },
    )
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub(crate) struct PluginVirtualTypePatch<'db> {
    name: String,
    ty: Type<'db>,
}

impl<'db> PluginVirtualTypePatch<'db> {
    fn new(name: String, ty: Type<'db>) -> Self {
        Self { name, ty }
    }
}

pub(crate) fn plugin_virtual_type_patches_from_protocol<'db>(
    db: &'db dyn Db,
    definitions: Vec<protocol::VirtualTypeDefinition>,
) -> Box<[PluginVirtualTypePatch<'db>]> {
    let scope = plugin_virtual_type_scope(db);
    definitions
        .into_iter()
        .filter_map(|definition| plugin_virtual_type_definition_to_patch(db, definition, scope))
        .collect()
}

fn plugin_virtual_type_definition_to_patch<'db>(
    db: &'db dyn Db,
    definition: protocol::VirtualTypeDefinition,
    scope: Option<ScopeId<'db>>,
) -> Option<PluginVirtualTypePatch<'db>> {
    let ty = match definition.shape {
        protocol::VirtualTypeShape::TypedDict { fields, total } => {
            let mut schema = TypedDictSchema::default();
            for field in fields {
                let declared_ty = plugin_type_expr_to_type(db, &field.type_expr);
                schema.insert(
                    Name::new(field.name),
                    TypedDictFieldBuilder::new(declared_ty)
                        .required(total && field.required)
                        .read_only(field.read_only)
                        .build(),
                );
            }
            Type::TypedDict(TypedDictType::from_schema_items_with_openness(
                db,
                schema,
                TypedDictOpenness::Closed,
            ))
        }
        protocol::VirtualTypeShape::NamedTuple { fields } => {
            let scope = scope?;
            let fields = fields
                .into_iter()
                .map(|field| NamedTupleField {
                    name: Name::new(field.name),
                    ty: plugin_type_expr_to_type(db, &field.type_expr),
                    default: None,
                    definition: None,
                })
                .collect::<Vec<_>>();
            let spec = NamedTupleSpec::known(db, fields.into_boxed_slice());
            let short_name = definition
                .name
                .rsplit('.')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or("PluginNamedTuple");
            let named_tuple = DynamicNamedTupleLiteral::new(
                db,
                &Name::new(short_name),
                DynamicNamedTupleAnchor::PluginVirtual {
                    scope,
                    identity: definition.name.clone(),
                    spec,
                },
            );
            named_tuple.to_instance(db)
        }
        protocol::VirtualTypeShape::Class { bases, members } => {
            let scope = scope?;
            let context = PluginTypeExprContext {
                scope: Some(scope),
                ..PluginTypeExprContext::default()
            };
            let explicit_bases = bases
                .iter()
                .map(|base| {
                    plugin_class_base_type_expr_to_type(db, base, context)
                        .unwrap_or_else(|| plugin_type_expr_to_type_with_context(db, base, context))
                })
                .collect::<Box<_>>();
            let (class_members, instance_members) =
                plugin_virtual_class_members(db, members.as_slice(), context);
            let class_members = class_members.iter().cloned().collect::<Box<_>>();
            let short_name = definition
                .name
                .rsplit('.')
                .next()
                .filter(|name| !name.is_empty())
                .unwrap_or("PluginVirtualClass");
            let dynamic_class = DynamicClassLiteral::new(
                db,
                &Name::new(short_name),
                &DynamicClassAnchor::PluginVirtual {
                    scope,
                    identity: definition.name.clone(),
                    explicit_bases,
                },
                &class_members,
                &instance_members,
                false,
                None,
            );
            Type::instance(
                db,
                ClassType::NonGeneric(ClassLiteral::Dynamic(dynamic_class)),
            )
        }
    };

    Some(PluginVirtualTypePatch::new(definition.name, ty))
}

fn plugin_virtual_class_members<'db>(
    db: &'db dyn Db,
    members: &[protocol::MemberPatch],
    context: PluginTypeExprContext<'db, '_>,
) -> (Box<[(Name, Type<'db>)]>, Box<[(Name, Type<'db>)]>) {
    let mut class_members = Vec::new();
    let mut instance_members = Vec::new();
    for member in members {
        let name = Name::new(member.name.clone());
        match &member.access {
            protocol::MemberAccessPatch::Value { type_expr } => {
                class_members.push((
                    name,
                    plugin_type_expr_to_type_with_context(db, type_expr, context),
                ));
            }
            protocol::MemberAccessPatch::Descriptor {
                class_type,
                instance_get_type,
                ..
            } => {
                if let Some(class_type) = class_type {
                    class_members.push((
                        name.clone(),
                        plugin_type_expr_to_type_with_context(db, class_type, context),
                    ));
                }
                instance_members.push((
                    name,
                    plugin_type_expr_to_type_with_context(db, instance_get_type, context),
                ));
            }
            protocol::MemberAccessPatch::Callable { signature, .. } => {
                instance_members.push((
                    name,
                    plugin_callable_type_from_protocol_signature(db, signature, context),
                ));
            }
        }
    }
    (
        class_members.into_boxed_slice(),
        instance_members.into_boxed_slice(),
    )
}

fn plugin_class_base_type_expr_to_type<'db>(
    db: &'db dyn Db,
    type_expr: &protocol::TypeExpr,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Type<'db>> {
    let context = PluginTypeExprContext {
        imports: &type_expr.imports,
        ..context
    };
    parse_plugin_class_base_expr(db, type_expr.expression.trim(), context)
}

fn parse_plugin_class_base_expr<'db>(
    db: &'db dyn Db,
    expression: &str,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Type<'db>> {
    let expression = strip_wrapping_parentheses(expression.trim());
    if let Some((origin, args)) = parse_generic_type_expr(expression) {
        let origin = imported_qualified_name(origin, context).unwrap_or_else(|| origin.to_string());
        let Type::ClassLiteral(class) = resolve_plugin_qualified_type_expr_value(db, &origin)?
        else {
            return None;
        };
        let generic_context = class.generic_context(db)?;
        if generic_context.len(db) != args.len() {
            return None;
        }

        let specialization_args = args
            .into_iter()
            .map(|arg| Some(parse_plugin_type_expr(db, arg, context).unwrap_or_else(Type::unknown)))
            .collect::<Vec<_>>();

        return Some(Type::from(
            class.apply_specialization(db, |generic_context| {
                generic_context.specialize_recursive(db, specialization_args)
            }),
        ));
    }

    let expression =
        imported_qualified_name(expression, context).unwrap_or_else(|| expression.to_string());
    resolve_plugin_qualified_type_expr_value(db, &expression)
}

fn plugin_virtual_type_scope<'db>(db: &'db dyn Db) -> Option<ScopeId<'db>> {
    all_modules(db)
        .into_iter()
        .filter_map(|module| module.file(db))
        .filter(|file| db.should_check_file(*file))
        .min_by_key(|file| file.path(db).to_string())
        .map(|file| global_scope(db, file))
}

#[derive(Clone, Copy)]
struct PluginTypeExprContext<'db, 'ctx> {
    self_class: Option<StaticClassLiteral<'db>>,
    self_type: Option<Type<'db>>,
    scope: Option<ScopeId<'db>>,
    virtual_types: &'ctx [PluginVirtualTypePatch<'db>],
    imports: &'ctx [protocol::ImportBinding],
}

impl<'db, 'ctx> Default for PluginTypeExprContext<'db, 'ctx> {
    fn default() -> Self {
        Self {
            self_class: None,
            self_type: None,
            scope: None,
            virtual_types: &[],
            imports: &[],
        }
    }
}

#[cfg(test)]
fn plugin_type_expr_to_type_in_file<'db>(
    db: &'db dyn Db,
    type_expr: &protocol::TypeExpr,
    file: File,
) -> Type<'db> {
    plugin_type_expr_to_type_with_context(
        db,
        type_expr,
        PluginTypeExprContext {
            self_class: None,
            scope: Some(global_scope(db, file)),
            ..PluginTypeExprContext::default()
        },
    )
}

fn plugin_type_expr_to_type_in_file_with_virtual_types<'db>(
    db: &'db dyn Db,
    type_expr: &protocol::TypeExpr,
    file: File,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> Type<'db> {
    plugin_type_expr_to_type_with_context(
        db,
        type_expr,
        PluginTypeExprContext {
            self_class: None,
            scope: Some(global_scope(db, file)),
            virtual_types,
            ..PluginTypeExprContext::default()
        },
    )
}

fn plugin_type_expr_to_type_with_context<'db>(
    db: &'db dyn Db,
    type_expr: &protocol::TypeExpr,
    context: PluginTypeExprContext<'db, '_>,
) -> Type<'db> {
    if let Some(snapshot) = type_expr.snapshot.as_deref()
        && let Some(ty) = plugin_type_snapshot_to_type(db, snapshot, context)
    {
        return ty;
    }
    let context = PluginTypeExprContext {
        imports: &type_expr.imports,
        ..context
    };
    parse_plugin_type_expr(db, type_expr.expression.trim(), context).unwrap_or_else(Type::unknown)
}

fn plugin_type_snapshot_to_type<'db>(
    db: &'db dyn Db,
    snapshot: &protocol::TypeSnapshot,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Type<'db>> {
    match snapshot {
        protocol::TypeSnapshot::Expression(expression) => {
            let type_expr = expression.to_type_expr();
            Some(plugin_type_expr_to_type_with_context(
                db, &type_expr, context,
            ))
        }
        protocol::TypeSnapshot::Nominal {
            qualified_name,
            arguments,
        } => {
            let Type::ClassLiteral(class) =
                resolve_plugin_qualified_type_expr_value(db, qualified_name)?
            else {
                return None;
            };
            if arguments.is_empty() {
                return Some(Type::instance(db, class.default_specialization(db)));
            }
            let generic_context = class.generic_context(db)?;
            if generic_context.len(db) != arguments.len() {
                return None;
            }
            let arguments = arguments
                .iter()
                .map(|argument| {
                    Some(
                        plugin_type_snapshot_to_type(db, argument, context)
                            .unwrap_or_else(Type::unknown),
                    )
                })
                .collect::<Vec<_>>();
            Some(Type::instance(
                db,
                class.apply_specialization(db, |generic_context| {
                    generic_context.specialize_recursive(db, arguments)
                }),
            ))
        }
        protocol::TypeSnapshot::Tuple {
            prefix,
            variadic,
            suffix,
        } => {
            let prefix = prefix
                .iter()
                .map(|element| {
                    plugin_type_snapshot_to_type(db, element, context).unwrap_or_else(Type::unknown)
                })
                .collect::<Vec<_>>();
            let suffix = suffix
                .iter()
                .map(|element| {
                    plugin_type_snapshot_to_type(db, element, context).unwrap_or_else(Type::unknown)
                })
                .collect::<Vec<_>>();
            if let Some(variadic) = variadic {
                let variadic = plugin_type_snapshot_to_type(db, variadic, context)
                    .unwrap_or_else(Type::unknown);
                Some(Type::tuple(TupleType::mixed(db, prefix, variadic, suffix)))
            } else {
                Some(Type::heterogeneous_tuple(
                    db,
                    prefix.into_iter().chain(suffix),
                ))
            }
        }
        protocol::TypeSnapshot::TypedDict {
            fields,
            extra_items,
            closed,
        } => {
            let schema = fields
                .iter()
                .map(|field| {
                    let declared_ty =
                        plugin_type_snapshot_to_type(db, &field.type_snapshot, context)
                            .unwrap_or_else(Type::unknown);
                    (
                        Name::new(&field.name),
                        TypedDictFieldBuilder::new(declared_ty)
                            .required(field.required)
                            .read_only(field.read_only)
                            .build(),
                    )
                })
                .collect::<TypedDictSchema<'db>>();
            let openness = if let Some(extra_items) = extra_items {
                let ty = plugin_type_snapshot_to_type(db, &extra_items.type_snapshot, context)
                    .unwrap_or_else(Type::unknown);
                TypedDictOpenness::extra(db, ty, extra_items.read_only)
            } else if *closed {
                TypedDictOpenness::Closed
            } else {
                TypedDictOpenness::ImplicitlyOpen
            };
            Some(Type::TypedDict(
                TypedDictType::from_schema_items_with_openness(db, schema, openness),
            ))
        }
        protocol::TypeSnapshot::Union { elements } => Some(UnionType::from_elements(
            db,
            elements.iter().map(|element| {
                plugin_type_snapshot_to_type(db, element, context).unwrap_or_else(Type::unknown)
            }),
        )),
        protocol::TypeSnapshot::PluginClass { identity } => {
            resolve_plugin_virtual_type_expr(identity, context)
                .or_else(|| parse_plugin_type_expr(db, identity, context))
        }
        protocol::TypeSnapshot::SelfType { bound } => {
            if let Some(self_type) = context.self_type {
                Some(self_type)
            } else if let Some(class) = context.self_class {
                Some(Type::instance(db, class.default_specialization(db)))
            } else {
                bound
                    .as_deref()
                    .and_then(|bound| plugin_type_snapshot_to_type(db, bound, context))
            }
        }
        protocol::TypeSnapshot::Annotated { base, .. } => {
            plugin_type_snapshot_to_type(db, base, context)
        }
    }
}

fn parse_plugin_type_expr<'db>(
    db: &'db dyn Db,
    expression: &str,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Type<'db>> {
    let expression = strip_wrapping_parentheses(expression.trim());
    if expression.is_empty() {
        return None;
    }

    let union_parts = split_top_level(expression, '|');
    if union_parts.len() > 1 {
        return Some(UnionType::from_elements(
            db,
            union_parts.into_iter().map(|part| {
                parse_plugin_type_expr(db, part, context).unwrap_or_else(Type::unknown)
            }),
        ));
    }

    if let Some(typed_dict) = parse_plugin_anonymous_typed_dict_type_expr(db, expression, context) {
        return Some(typed_dict);
    }
    if let Some(named_tuple) = parse_plugin_named_tuple_type_expr(db, expression, context) {
        return Some(named_tuple);
    }
    if let Some(class) = parse_plugin_class_type_expr(db, expression, context) {
        return Some(class);
    }

    if let Some((origin, args)) = parse_generic_type_expr(expression) {
        return parse_plugin_generic_type_expr(db, origin, args, context);
    }

    parse_plugin_atomic_type_expr(db, expression, context)
}

fn parse_plugin_atomic_type_expr<'db>(
    db: &'db dyn Db,
    expression: &str,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Type<'db>> {
    if let Some(ty) = resolve_plugin_virtual_type_expr(expression, context) {
        return Some(ty);
    }

    Some(match expression {
        "Any" | "typing.Any" | "typing_extensions.Any" => Type::any(),
        "bool" | "builtins.bool" => KnownClass::Bool.to_instance(db),
        "bytes" | "builtins.bytes" => KnownClass::Bytes.to_instance(db),
        "dict" | "builtins.dict" => KnownClass::Dict.to_instance(db),
        "float" | "builtins.float" => KnownClass::Float.to_instance(db),
        "int" | "builtins.int" => KnownClass::Int.to_instance(db),
        "list" | "builtins.list" => KnownClass::List.to_instance(db),
        "None" | "NoneType" | "types.NoneType" | "builtins.None" => Type::none(db),
        "object" | "builtins.object" => KnownClass::Object.to_instance(db),
        "Self" | "typing.Self" | "typing_extensions.Self" => {
            Type::instance(db, context.self_class?.default_specialization(db))
        }
        "str" | "builtins.str" => KnownClass::Str.to_instance(db),
        _ => {
            return resolve_plugin_qualified_type_expr(db, expression).or_else(|| {
                imported_qualified_name(expression, context).and_then(|qualified_name| {
                    resolve_plugin_qualified_type_expr(db, &qualified_name)
                })
            });
        }
    })
}

fn parse_plugin_generic_type_expr<'db>(
    db: &'db dyn Db,
    origin: &str,
    args: Vec<&str>,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Type<'db>> {
    match origin {
        "type" | "builtins.type" | "typing.Type" if args.len() == 1 => {
            let instance = parse_plugin_type_expr(db, args[0], context)?;
            SubclassOfType::try_from_instance(db, instance)
        }
        "Optional" | "typing.Optional" | "typing_extensions.Optional" if args.len() == 1 => {
            Some(UnionType::from_elements(
                db,
                [
                    parse_plugin_type_expr(db, args[0], context).unwrap_or_else(Type::unknown),
                    Type::none(db),
                ],
            ))
        }
        "Union" | "typing.Union" | "typing_extensions.Union" if !args.is_empty() => {
            Some(UnionType::from_elements(
                db,
                args.into_iter().map(|arg| {
                    parse_plugin_type_expr(db, arg, context).unwrap_or_else(Type::unknown)
                }),
            ))
        }
        "Literal" | "typing.Literal" | "typing_extensions.Literal" if !args.is_empty() => {
            Some(UnionType::from_elements(
                db,
                args.into_iter().map(|arg| {
                    parse_plugin_literal_type_arg(db, arg).unwrap_or_else(Type::unknown)
                }),
            ))
        }
        "list" | "builtins.list" | "typing.List" if args.len() == 1 => {
            let specialization =
                [parse_plugin_type_expr(db, args[0], context).unwrap_or_else(Type::unknown)];
            Some(KnownClass::List.to_specialized_instance(db, &specialization))
        }
        "dict" | "builtins.dict" | "typing.Dict" if args.len() == 2 => {
            let specialization = [
                parse_plugin_type_expr(db, args[0], context).unwrap_or_else(Type::unknown),
                parse_plugin_type_expr(db, args[1], context).unwrap_or_else(Type::unknown),
            ];
            Some(KnownClass::Dict.to_specialized_instance(db, &specialization))
        }
        "tuple" | "builtins.tuple" | "typing.Tuple" if args.len() == 2 && args[1] == "..." => {
            Some(Type::homogeneous_tuple(
                db,
                parse_plugin_type_expr(db, args[0], context).unwrap_or_else(Type::unknown),
            ))
        }
        "tuple" | "builtins.tuple" | "typing.Tuple" => Some(Type::heterogeneous_tuple(
            db,
            args.into_iter()
                .map(|arg| parse_plugin_type_expr(db, arg, context).unwrap_or_else(Type::unknown)),
        )),
        _ => parse_plugin_project_generic_type_expr(db, origin, args, context),
    }
}

fn parse_plugin_literal_type_arg<'db>(db: &'db dyn Db, expression: &str) -> Option<Type<'db>> {
    let expression = expression.trim();
    let parsed = parse_expression(expression).ok()?.into_expr();
    parse_plugin_literal_expr(db, &parsed)
}

fn parse_plugin_literal_expr<'db>(db: &'db dyn Db, expression: &ast::Expr) -> Option<Type<'db>> {
    Some(match expression {
        ast::Expr::BooleanLiteral(boolean) => Type::bool_literal(boolean.value),
        ast::Expr::NoneLiteral(_) => Type::none(db),
        ast::Expr::StringLiteral(string) => Type::string_literal(db, string.value.to_str()),
        ast::Expr::BytesLiteral(bytes) => {
            let value = bytes.value.bytes().collect::<Vec<_>>();
            Type::bytes_literal(db, &value)
        }
        ast::Expr::NumberLiteral(number) => match &number.value {
            ast::Number::Int(int) => Type::int_literal(int.as_i64()?),
            ast::Number::Float(_) | ast::Number::Complex { .. } => return None,
        },
        ast::Expr::UnaryOp(unary) => {
            let ast::Expr::NumberLiteral(number) = unary.operand.as_ref() else {
                return None;
            };
            let ast::Number::Int(int) = &number.value else {
                return None;
            };
            let value = int.as_i64()?;
            match unary.op {
                ast::UnaryOp::UAdd => Type::int_literal(value),
                ast::UnaryOp::USub => Type::int_literal(value.checked_neg()?),
                ast::UnaryOp::Invert | ast::UnaryOp::Not => return None,
            }
        }
        ast::Expr::Name(_) | ast::Expr::Attribute(_) => {
            let qualified_name = plugin_symbol_ref_from_expr(expression)?.qualified_name;
            let ty = resolve_plugin_qualified_type_expr_value(db, &qualified_name)?;
            if matches!(ty, Type::LiteralValue(literal) if literal.is_enum()) {
                ty
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

fn parse_plugin_anonymous_typed_dict_type_expr<'db>(
    db: &'db dyn Db,
    expression: &str,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Type<'db>> {
    let fields_expression = typed_dict_call_fields_expression(expression)?;
    let fields_expression = fields_expression
        .strip_prefix('{')?
        .strip_suffix('}')?
        .trim();
    let mut schema = TypedDictSchema::default();

    if !fields_expression.is_empty() {
        for entry in split_top_level(fields_expression, ',') {
            let (key_expression, type_expression) = split_once_top_level(entry, ':')?;
            let key = parse_string_literal_key(key_expression)?;
            let declared_ty =
                parse_plugin_type_expr(db, type_expression, context).unwrap_or_else(Type::unknown);
            schema.insert(
                Name::new(key),
                TypedDictFieldBuilder::new(declared_ty)
                    .required(true)
                    .build(),
            );
        }
    }

    Some(Type::TypedDict(
        TypedDictType::from_schema_items_with_openness(db, schema, TypedDictOpenness::Closed),
    ))
}

fn typed_dict_call_fields_expression(expression: &str) -> Option<&str> {
    let (origin, rest) = expression.split_once('(')?;
    if !matches!(
        origin.trim(),
        "TypedDict" | "typing.TypedDict" | "typing_extensions.TypedDict"
    ) || !rest.ends_with(')')
    {
        return None;
    }
    Some(rest[..rest.len() - 1].trim())
}

fn parse_string_literal_key(expression: &str) -> Option<String> {
    let parsed = parse_expression(expression.trim()).ok()?.into_expr();
    let ast::Expr::StringLiteral(string) = parsed else {
        return None;
    };
    Some(string.value.to_str().to_string())
}

fn parse_plugin_named_tuple_type_expr<'db>(
    db: &'db dyn Db,
    expression: &str,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Type<'db>> {
    let arguments = named_tuple_call_arguments(expression)?;
    let parts = split_top_level(arguments, ',');
    let (name, fields_expression) = match parts.as_slice() {
        [fields_expression] => ("PluginNamedTuple".to_string(), *fields_expression),
        [name_expression, fields_expression] => (
            parse_string_literal_key(name_expression)?,
            *fields_expression,
        ),
        _ => return None,
    };
    let fields_expression = fields_expression
        .strip_prefix('{')?
        .strip_suffix('}')?
        .trim();

    let mut fields = Vec::new();
    if !fields_expression.is_empty() {
        for entry in split_top_level(fields_expression, ',') {
            let (key_expression, type_expression) = split_once_top_level(entry, ':')?;
            let key = parse_string_literal_key(key_expression)?;
            fields.push(NamedTupleField {
                name: Name::new(key),
                ty: parse_plugin_type_expr(db, type_expression, context)
                    .unwrap_or_else(Type::unknown),
                default: None,
                definition: None,
            });
        }
    }

    let scope = context.scope?;
    let spec = NamedTupleSpec::known(db, fields.into_boxed_slice());
    let name = Name::new(name);
    let named_tuple = DynamicNamedTupleLiteral::new(
        db,
        &name,
        DynamicNamedTupleAnchor::PluginVirtual {
            scope,
            identity: expression.to_string(),
            spec,
        },
    );
    Some(named_tuple.to_instance(db))
}

fn named_tuple_call_arguments(expression: &str) -> Option<&str> {
    let (origin, rest) = expression.split_once('(')?;
    if !matches!(
        origin.trim(),
        "NamedTuple" | "typing.NamedTuple" | "typing_extensions.NamedTuple"
    ) || !rest.ends_with(')')
    {
        return None;
    }
    Some(rest[..rest.len() - 1].trim())
}

fn parse_plugin_class_type_expr<'db>(
    db: &'db dyn Db,
    expression: &str,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Type<'db>> {
    let arguments = class_call_arguments(expression)?;
    let parts = split_top_level(arguments, ',');
    let [name_expression, members_expression, base_expression] = parts.as_slice() else {
        return None;
    };
    let name = parse_string_literal_key(name_expression)?;
    let members_expression = members_expression
        .strip_prefix('{')?
        .strip_suffix('}')?
        .trim();

    let mut instance_members = Vec::new();
    if !members_expression.is_empty() {
        for entry in split_top_level(members_expression, ',') {
            let (key_expression, type_expression) = split_once_top_level(entry, ':')?;
            let key = parse_string_literal_key(key_expression)?;
            instance_members.push((
                Name::new(key),
                parse_plugin_type_expr(db, type_expression, context).unwrap_or_else(Type::unknown),
            ));
        }
    }

    let base_expr = protocol::TypeExpr {
        expression: base_expression.trim().to_string(),
        mode: protocol::TypeExprMode::Expression,
        imports: context.imports.to_vec(),
        snapshot: None,
    };
    let explicit_bases = Box::from(
        [plugin_class_base_type_expr_to_type(db, &base_expr, context)
            .unwrap_or_else(|| plugin_type_expr_to_type_with_context(db, &base_expr, context))],
    );
    let scope = context.scope?;
    let class_members: Box<[(Name, Type<'db>)]> = Box::default();
    let instance_members = instance_members.into_boxed_slice();
    let dynamic_class = DynamicClassLiteral::new(
        db,
        &Name::new(name),
        &DynamicClassAnchor::PluginVirtual {
            scope,
            identity: expression.to_string(),
            explicit_bases,
        },
        &class_members,
        &instance_members,
        false,
        None,
    );
    Some(Type::instance(
        db,
        ClassType::NonGeneric(ClassLiteral::Dynamic(dynamic_class)),
    ))
}

fn class_call_arguments(expression: &str) -> Option<&str> {
    let (origin, rest) = expression.split_once('(')?;
    if !matches!(origin.trim(), "Class" | "PluginClass") || !rest.ends_with(')') {
        return None;
    }
    Some(rest[..rest.len() - 1].trim())
}

fn parse_plugin_project_generic_type_expr<'db>(
    db: &'db dyn Db,
    origin: &str,
    args: Vec<&str>,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Type<'db>> {
    let origin = imported_qualified_name(origin, context).unwrap_or_else(|| origin.to_string());
    let Type::ClassLiteral(class) = resolve_plugin_qualified_type_expr_value(db, &origin)? else {
        return None;
    };

    let generic_context = class.generic_context(db)?;
    if generic_context.len(db) != args.len() {
        return None;
    }

    let specialization_args = args
        .into_iter()
        .map(|arg| Some(parse_plugin_type_expr(db, arg, context).unwrap_or_else(Type::unknown)))
        .collect::<Vec<_>>();

    Some(Type::instance(
        db,
        class.apply_specialization(db, |generic_context| {
            generic_context.specialize_recursive(db, specialization_args)
        }),
    ))
}

fn resolve_plugin_virtual_type_expr<'db>(
    expression: &str,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Type<'db>> {
    let imported_name = imported_qualified_name(expression, context);
    context
        .virtual_types
        .iter()
        .find(|virtual_type| {
            virtual_type.name == expression
                || imported_name
                    .as_deref()
                    .is_some_and(|qualified_name| virtual_type.name == qualified_name)
        })
        .map(|virtual_type| virtual_type.ty)
}

fn imported_qualified_name(
    expression: &str,
    context: PluginTypeExprContext<'_, '_>,
) -> Option<String> {
    context.imports.iter().find_map(|binding| {
        let local_name = binding.alias.as_deref().unwrap_or(&binding.name);
        (local_name == expression).then(|| format!("{}.{}", binding.module, binding.name))
    })
}

fn resolve_plugin_qualified_type_expr<'db>(db: &'db dyn Db, expression: &str) -> Option<Type<'db>> {
    resolve_plugin_qualified_type_expr_value(db, expression)
        .map(|ty| plugin_annotation_type_from_value(db, ty))
}

fn resolve_plugin_qualified_type_expr_value<'db>(
    db: &'db dyn Db,
    expression: &str,
) -> Option<Type<'db>> {
    let components = expression.split('.').collect::<Vec<_>>();
    if components.len() < 2 || components.iter().any(|component| component.is_empty()) {
        return None;
    }

    for symbol_start in (1..components.len()).rev() {
        let module_name = ModuleName::new(&components[..symbol_start].join("."))?;
        let Some(module) = resolve_module_confident(db, &module_name) else {
            continue;
        };

        let mut ty = imported_symbol(db, module.file(db), components[symbol_start], None)
            .ignore_possibly_undefined()?;

        for member in &components[symbol_start + 1..] {
            ty = ty.member(db, member).ignore_possibly_undefined()?;
        }

        return Some(ty);
    }

    None
}

fn plugin_annotation_type_from_value<'db>(db: &'db dyn Db, ty: Type<'db>) -> Type<'db> {
    match ty {
        Type::ClassLiteral(class) => Type::instance(db, class.default_specialization(db)),
        Type::GenericAlias(alias) => Type::instance(db, ClassType::from(alias)),
        _ => ty,
    }
}

fn parse_generic_type_expr(expression: &str) -> Option<(&str, Vec<&str>)> {
    let open = expression.find('[')?;
    if !expression.ends_with(']') {
        return None;
    }

    let origin = expression[..open].trim();
    if origin.is_empty() {
        return None;
    }

    let args = &expression[open + 1..expression.len() - 1];
    Some((origin, split_top_level(args, ',')))
}

fn split_top_level(expression: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (offset, character) in expression.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            continue;
        }

        match character {
            '\'' | '"' => quote = Some(character),
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = depth.saturating_sub(1),
            character if character == delimiter && depth == 0 => {
                parts.push(expression[start..offset].trim());
                start = offset + character.len_utf8();
            }
            _ => {}
        }
    }

    parts.push(expression[start..].trim());
    parts
}

fn split_once_top_level(expression: &str, delimiter: char) -> Option<(&str, &str)> {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (offset, character) in expression.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            continue;
        }

        match character {
            '\'' | '"' => quote = Some(character),
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = depth.saturating_sub(1),
            character if character == delimiter && depth == 0 => {
                return Some((
                    expression[..offset].trim(),
                    expression[offset + character.len_utf8()..].trim(),
                ));
            }
            _ => {}
        }
    }

    None
}

fn strip_wrapping_parentheses(expression: &str) -> &str {
    let mut current = expression;
    while let Some(inner) = current
        .strip_prefix('(')
        .and_then(|expr| expr.strip_suffix(')'))
    {
        if split_top_level(inner, '|').len() == 1 || parentheses_wrap_entire_expression(current) {
            current = inner.trim();
        } else {
            break;
        }
    }
    current
}

fn parentheses_wrap_entire_expression(expression: &str) -> bool {
    let mut depth = 0usize;
    for (offset, character) in expression.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 && offset + character.len_utf8() != expression.len() {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Build the semantic context sent to plugins for a hook rooted in `file`.
pub(crate) fn plugin_semantic_context(
    db: &dyn Db,
    file: File,
    speculative: bool,
) -> protocol::SemanticContext {
    let module = file_to_module(db, file)
        .map(|module| module.name(db).to_string())
        .unwrap_or_default();

    protocol::SemanticContext {
        module,
        file_path: file.path(db).to_string(),
        python_version: Program::get(db).python_version(db).to_string(),
        platform: Program::get(db).python_platform(db).to_string(),
        speculative,
    }
}

/// The kind of callee a call-site hook is being applied to.
enum PluginCallee<'db> {
    /// A function or bound-method call. A signature patch's return type is honored as-is.
    Callable {
        qualified_name: String,
        method_name: Option<String>,
        receiver_ty: Option<Type<'db>>,
    },
    /// A constructor call. A signature patch adjusts the accepted arguments, but the call still
    /// yields an instance of the constructed class, so we keep the instance type as the return.
    Constructor {
        qualified_name: String,
        instance_ty: Type<'db>,
    },
}

impl<'db> PluginCallee<'db> {
    fn qualified_name(&self) -> &str {
        match self {
            PluginCallee::Callable { qualified_name, .. }
            | PluginCallee::Constructor { qualified_name, .. } => qualified_name,
        }
    }

    fn receiver_ty(&self) -> Option<Type<'db>> {
        match self {
            PluginCallee::Callable { receiver_ty, .. } => *receiver_ty,
            PluginCallee::Constructor { .. } => None,
        }
    }

    fn method_name(&self) -> Option<&str> {
        match self {
            PluginCallee::Callable { method_name, .. } => method_name.as_deref(),
            PluginCallee::Constructor { .. } => None,
        }
    }
}

/// Identify the callee of a call expression for plugin routing, recovering its qualified name.
///
/// Only functions, bound methods, and class constructors are routed; other callables are left
/// untouched.
fn plugin_callee<'db>(db: &'db dyn Db, callable_type: Type<'db>) -> Option<PluginCallee<'db>> {
    match callable_type {
        Type::FunctionLiteral(function) => Some(PluginCallee::Callable {
            qualified_name: function_qualified_name(db, function),
            method_name: None,
            receiver_ty: None,
        }),
        Type::BoundMethod(method) => Some(PluginCallee::Callable {
            qualified_name: function_qualified_name(db, method.function(db)),
            method_name: Some(method.function(db).name(db).to_string()),
            receiver_ty: Some(method.self_instance(db)),
        }),
        Type::ClassLiteral(class) => Some(PluginCallee::Constructor {
            qualified_name: class.qualified_name(db).to_string(),
            instance_ty: class.to_non_generic_instance(db),
        }),
        _ => None,
    }
}

/// Compute the dotted qualified name of a function, e.g. `pkg.mod.Class.method`.
fn function_qualified_name<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> String {
    let file = function.file(db);
    let file_scope_id = function.last_definition(db).scope(db).file_scope_id(db);
    let mut components = qualified_name_components_from_scope(db, file, file_scope_id, 0);
    components.push(function.name(db).to_string());
    components.join(".")
}

/// Runtime failure information that can be reported by inference callers with source context.
#[derive(Debug, Clone)]
pub(crate) struct PluginRuntimeDiagnostic {
    plugin_id: String,
    error: SemanticPluginRuntimeError,
}

impl PluginRuntimeDiagnostic {
    fn new(plugin_id: impl Into<String>, error: SemanticPluginRuntimeError) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            error,
        }
    }

    pub(crate) fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub(crate) fn message(&self) -> &str {
        self.error.message()
    }

    pub(crate) fn hint(&self) -> &str {
        self.error.hint()
    }
}

/// A call-return adjustment produced by a plugin.
pub(crate) struct PluginCallReturnAdjustment<'db> {
    return_ty: Type<'db>,
    diagnostics: Vec<protocol::PluginDiagnostic>,
}

impl<'db> PluginCallReturnAdjustment<'db> {
    fn new(return_ty: Type<'db>, diagnostics: Vec<protocol::PluginDiagnostic>) -> Self {
        Self {
            return_ty,
            diagnostics,
        }
    }

    pub(crate) fn return_ty(&self) -> Type<'db> {
        self.return_ty
    }

    pub(crate) fn diagnostics(&self) -> &[protocol::PluginDiagnostic] {
        &self.diagnostics
    }
}

/// Returns the first enabled plugin claiming `qualified_name` for the given hook kind.
fn matching_call_plugin<'a>(
    db: &dyn Db,
    plugins: &'a [SemanticPlugin],
    callee: &PluginCallee<'_>,
    hook: CallHook,
) -> Option<&'a SemanticPlugin> {
    plugins.iter().find(|plugin| {
        let claims = match hook {
            CallHook::Signature => plugin.call_signature_claims(),
            CallHook::Return => plugin.call_return_claims(),
        };
        if claims.iter().any(|claim| claim == callee.qualified_name()) {
            return true;
        }

        let method_claims = match hook {
            CallHook::Signature => plugin.call_signature_method_on_subclass_claims(),
            CallHook::Return => plugin.call_return_method_on_subclass_claims(),
        };

        method_claims.iter().any(|claim| {
            callee.method_name() == Some(claim.method_name())
                && callee_receiver_is_subclass_of(db, callee, claim.base_qualified_name())
        })
    })
}

fn callee_receiver_is_subclass_of(
    db: &dyn Db,
    callee: &PluginCallee<'_>,
    base_qualified_name: &str,
) -> bool {
    let Some(receiver_class) = callee
        .receiver_ty()
        .and_then(|receiver| receiver.nominal_class(db))
    else {
        return false;
    };
    let Some(Type::ClassLiteral(base_class)) =
        resolve_plugin_qualified_type_expr_value(db, base_qualified_name)
    else {
        return false;
    };

    receiver_class.is_subtype_of_class_literal(db, base_class)
}

#[derive(Clone, Copy)]
enum CallHook {
    Signature,
    Return,
}

/// If a plugin claims the call signature of `callable_type`, return a replacement callable to
/// bind and check arguments against instead of the original callee.
///
/// The signature hook runs before parameter matching, so replacing the callee here lets normal
/// argument inference and checking flow through the plugin-provided parameters unchanged.
pub(crate) fn plugin_adjusted_call_callable<'db>(
    db: &'db dyn Db,
    file: File,
    callable_type: Type<'db>,
    arguments: &ast::Arguments,
    speculative: bool,
) -> Result<Option<Type<'db>>, PluginRuntimeDiagnostic> {
    let semantic_plugins = Program::get(db).semantic_plugins(db);
    if semantic_plugins.is_empty() {
        return Ok(None);
    }

    let Some(callee) = plugin_callee(db, callable_type) else {
        return Ok(None);
    };
    let Some(plugin) =
        matching_call_plugin(db, semantic_plugins.plugins(), &callee, CallHook::Signature)
    else {
        return Ok(None);
    };

    tracing::trace!(
        plugin_id = plugin.id(),
        runtime = ?plugin.runtime(),
        callee = callee.qualified_name(),
        "executing call-signature plugin"
    );

    let request = plugin_call_request(
        db,
        plugin,
        file,
        &callee,
        arguments,
        None,
        None,
        CallHook::Signature,
        speculative,
    );
    let protocol::PluginResponse::CallSignaturePatch(patch) =
        execute_call_plugin(db, plugin, &request, CallHook::Signature)?
    else {
        return Ok(None);
    };
    let virtual_types = plugin_project_index_virtual_types(db, plugin);

    let parameters = patch
        .signature
        .parameters
        .iter()
        .filter_map(|parameter| plugin_protocol_parameter(db, parameter, virtual_types))
        .collect::<Vec<_>>();
    let return_ty = match &callee {
        PluginCallee::Constructor { instance_ty, .. } => *instance_ty,
        PluginCallee::Callable { .. } => plugin_type_expr_to_type_in_file_with_virtual_types(
            db,
            &patch.signature.return_type,
            file,
            virtual_types,
        ),
    };

    let signature = Signature::new(
        Parameters::new(parameters, ParametersKind::Standard),
        return_ty,
    );
    Ok(Some(Type::single_callable(db, signature)))
}

/// If a plugin claims the call return of `callable_type`, return the replacement return type.
///
/// The caller is responsible for not applying this to `TypeIs`/`TypeGuard` returns so that
/// narrowing stays intact.
pub(crate) fn plugin_adjusted_call_return<'db>(
    db: &'db dyn Db,
    file: File,
    callable_type: Type<'db>,
    arguments: &ast::Arguments,
    call_arguments: &CallArguments<'_, 'db>,
    default_return_ty: Type<'db>,
    speculative: bool,
) -> Result<Option<PluginCallReturnAdjustment<'db>>, PluginRuntimeDiagnostic> {
    let semantic_plugins = Program::get(db).semantic_plugins(db);
    if semantic_plugins.is_empty() {
        return Ok(None);
    }

    let Some(callee) = plugin_callee(db, callable_type) else {
        return Ok(None);
    };
    let Some(plugin) =
        matching_call_plugin(db, semantic_plugins.plugins(), &callee, CallHook::Return)
    else {
        return Ok(None);
    };

    tracing::trace!(
        plugin_id = plugin.id(),
        runtime = ?plugin.runtime(),
        callee = callee.qualified_name(),
        "executing call-return plugin"
    );

    let request = plugin_call_request(
        db,
        plugin,
        file,
        &callee,
        arguments,
        Some(call_arguments),
        Some(default_return_ty),
        CallHook::Return,
        speculative,
    );
    tracing::trace!(
        plugin_id = plugin.id(),
        ?request,
        "built call-return plugin request"
    );
    let response = execute_call_plugin(db, plugin, &request, CallHook::Return)?;
    let protocol::PluginResponse::CallReturnPatch(patch) = response else {
        return Ok(None);
    };
    let virtual_types = plugin_project_index_virtual_types(db, plugin);

    Ok(Some(PluginCallReturnAdjustment::new(
        plugin_type_expr_to_type_with_context(
            db,
            &patch.return_type,
            PluginTypeExprContext {
                self_type: callee.receiver_ty(),
                scope: Some(global_scope(db, file)),
                virtual_types,
                ..PluginTypeExprContext::default()
            },
        ),
        patch.diagnostics,
    )))
}

/// Run the first semantic plugin claiming mutation validation for `receiver_ty`.
pub(crate) fn plugin_mutation_diagnostics<'db>(
    db: &'db dyn Db,
    file: File,
    receiver_ty: Type<'db>,
    operation: protocol::MutationOperation,
    key: Option<&ast::Expr>,
    value: Option<&ast::Expr>,
    source_range: TextRange,
    speculative: bool,
) -> Result<Vec<protocol::PluginDiagnostic>, PluginRuntimeDiagnostic> {
    let semantic_plugins = Program::get(db).semantic_plugins(db);
    let Some(receiver_class) = receiver_ty.nominal_class(db) else {
        return Ok(Vec::new());
    };
    let receiver_qualified_name = receiver_class.qualified_name(db).to_string();

    let plugin = semantic_plugins.plugins().iter().find(|plugin| {
        if plugin
            .mutation_class_claims()
            .iter()
            .any(|claim| claim == &receiver_qualified_name)
        {
            return true;
        }

        plugin.mutation_subclass_claims().iter().any(|claim| {
            let Some(Type::ClassLiteral(base_class)) =
                resolve_plugin_qualified_type_expr_value(db, claim)
            else {
                return false;
            };
            receiver_class.is_subtype_of_class_literal(db, base_class)
        })
    });
    let Some(plugin) = plugin else {
        return Ok(Vec::new());
    };

    let argument = |expression: &ast::Expr| protocol::ArgumentSummary {
        name: None,
        kind: protocol::ArgumentKind::Positional,
        type_expr: None,
        value: plugin_literal_value_from_expr(expression),
        source: Some(plugin_symbol_source(db, file, expression.range(), None)),
    };
    let request = protocol::PluginRequest::ValidateMutation(protocol::MutationRequest {
        context: plugin_semantic_context(db, file, speculative),
        operation,
        receiver: plugin_qualified_type_expr_from_type(db, receiver_ty),
        key: key.map(argument),
        value: value.map(argument),
        source: plugin_symbol_source(db, file, source_range, None),
        project_index: plugin_project_index_json(db, plugin),
    });

    let response = match plugin.runtime() {
        SemanticPluginRuntime::Mock => protocol::PluginResponse::NoChange,
        SemanticPluginRuntime::InProcess | SemanticPluginRuntime::Wasm => db
            .execute_semantic_plugin(plugin.id(), &request)
            .map_err(|error| PluginRuntimeDiagnostic::new(plugin.id(), error))?,
    };
    let protocol::PluginResponse::MutationDiagnostics(response) = response else {
        return Ok(Vec::new());
    };
    Ok(response.diagnostics)
}

fn plugin_call_request<'db>(
    db: &'db dyn Db,
    plugin: &SemanticPlugin,
    file: File,
    callee: &PluginCallee<'db>,
    arguments: &ast::Arguments,
    call_arguments: Option<&CallArguments<'_, 'db>>,
    default_return_ty: Option<Type<'db>>,
    hook: CallHook,
    speculative: bool,
) -> protocol::PluginRequest {
    let request = protocol::CallRequest {
        context: plugin_semantic_context(db, file, speculative),
        callee: protocol::TypeExpr::expression(callee.qualified_name()),
        receiver: callee
            .receiver_ty()
            .map(|receiver_ty| plugin_receiver_summary(db, receiver_ty)),
        arguments: plugin_call_argument_summaries(db, file, arguments, call_arguments),
        existing_signature: None,
        default_return_type: default_return_ty.map(|ty| plugin_type_expr_from_type(db, ty)),
        project_index: plugin_project_index_json(db, plugin),
    };

    match hook {
        CallHook::Signature => protocol::PluginRequest::AdjustCallSignature(request),
        CallHook::Return => protocol::PluginRequest::AdjustCallReturn(request),
    }
}

fn plugin_receiver_summary<'db>(
    db: &'db dyn Db,
    receiver_ty: Type<'db>,
) -> protocol::ReceiverSummary {
    let summary_class = plugin_receiver_summary_class(db, receiver_ty);
    let nominal_class = summary_class.map(|class| class.qualified_name(db).to_string());
    let generic_arguments = summary_class
        .and_then(|class| match class {
            ClassType::Generic(generic) => Some(generic.specialization(db)),
            ClassType::NonGeneric(_) => None,
        })
        .map(|specialization| {
            specialization
                .types(db)
                .iter()
                .map(|ty| plugin_qualified_type_expr_from_type(db, *ty))
                .collect()
        })
        .unwrap_or_default();

    protocol::ReceiverSummary {
        type_expr: plugin_qualified_type_expr_from_type(db, receiver_ty),
        nominal_class,
        generic_arguments,
        plugin_metadata: Default::default(),
    }
}

fn plugin_receiver_summary_class<'db>(
    db: &'db dyn Db,
    receiver_ty: Type<'db>,
) -> Option<ClassType<'db>> {
    let receiver_class = receiver_ty.nominal_class(db)?;
    if receiver_class.is_generic() {
        return Some(receiver_class);
    }

    receiver_class
        .class_literal(db)
        .iter_mro(db)
        .find_map(|base| match base {
            ClassBase::Class(class @ ClassType::Generic(_)) => Some(class),
            _ => None,
        })
        .or(Some(receiver_class))
}

fn plugin_qualified_type_expr_from_type<'db>(db: &'db dyn Db, ty: Type<'db>) -> protocol::TypeExpr {
    if let Some(class) = ty.nominal_class(db) {
        if let ClassLiteral::Dynamic(dynamic_class) = class.class_literal(db)
            && let Some(identity) = dynamic_class.plugin_virtual_identity(db)
        {
            return protocol::TypeExpr::annotation(identity.to_string())
                .with_snapshot(plugin_type_snapshot_from_type(db, ty));
        }

        let mut expression = class.qualified_name(db).to_string();
        if let ClassType::Generic(generic) = class {
            let arguments = generic
                .specialization(db)
                .types(db)
                .iter()
                .map(|ty| plugin_qualified_type_expr_from_type(db, *ty).expression)
                .collect::<Vec<_>>();
            if !arguments.is_empty() {
                expression.push('[');
                expression.push_str(&arguments.join(", "));
                expression.push(']');
            }
        }
        return protocol::TypeExpr::annotation(expression)
            .with_snapshot(plugin_type_snapshot_from_type(db, ty));
    }

    plugin_type_expr_from_type(db, ty)
}

fn plugin_call_argument_summaries<'db>(
    db: &'db dyn Db,
    file: File,
    arguments: &ast::Arguments,
    call_arguments: Option<&CallArguments<'_, 'db>>,
) -> Vec<protocol::ArgumentSummary> {
    arguments
        .iter_source_order()
        .enumerate()
        .map(|(index, argument)| {
            let (name, kind, expression, range) = match argument {
                ArgOrKeyword::Arg(argument) => {
                    let kind = if argument.is_starred_expr() {
                        protocol::ArgumentKind::StarArgs
                    } else {
                        protocol::ArgumentKind::Positional
                    };
                    (None, kind, argument, argument.range())
                }
                ArgOrKeyword::Keyword(keyword) => (
                    keyword.arg.as_ref().map(|arg| arg.as_str().to_string()),
                    if keyword.arg.is_some() {
                        protocol::ArgumentKind::Keyword
                    } else {
                        protocol::ArgumentKind::StarKwargs
                    },
                    &keyword.value,
                    keyword.range(),
                ),
            };

            protocol::ArgumentSummary {
                name,
                kind,
                type_expr: call_arguments
                    .and_then(|arguments| arguments.argument_types(index))
                    .and_then(|types| types.get_default())
                    .map(|ty| plugin_type_expr_from_type(db, ty)),
                value: plugin_literal_value_from_expr(expression),
                source: Some(plugin_symbol_source(db, file, range, None)),
            }
        })
        .collect()
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
        module: file_to_module(db, file)
            .map(|module| module.name(db).to_string())
            .unwrap_or_default()
            .into(),
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

fn execute_call_plugin(
    db: &dyn Db,
    plugin: &SemanticPlugin,
    request: &protocol::PluginRequest,
    hook: CallHook,
) -> Result<protocol::PluginResponse, PluginRuntimeDiagnostic> {
    match plugin.runtime() {
        SemanticPluginRuntime::Mock => Ok(match hook {
            CallHook::Signature => mock_plugin_execute_call_signature(request),
            CallHook::Return => mock_plugin_execute_call_return(request),
        }),
        SemanticPluginRuntime::InProcess | SemanticPluginRuntime::Wasm => db
            .execute_semantic_plugin(plugin.id(), request)
            .map_err(|error| PluginRuntimeDiagnostic::new(plugin.id(), error)),
    }
}

fn plugin_protocol_parameter<'db>(
    db: &'db dyn Db,
    parameter: &protocol::Parameter,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> Option<Parameter<'db>> {
    plugin_protocol_parameter_with_context(
        db,
        parameter,
        PluginTypeExprContext {
            virtual_types,
            ..PluginTypeExprContext::default()
        },
    )
}

fn plugin_protocol_parameter_with_context<'db>(
    db: &'db dyn Db,
    parameter: &protocol::Parameter,
    context: PluginTypeExprContext<'db, '_>,
) -> Option<Parameter<'db>> {
    let name = parameter.name.as_ref().map(Name::new);
    let ty = parameter
        .type_expr
        .as_ref()
        .map_or_else(Type::unknown, |type_expr| {
            plugin_type_expr_to_type_with_context(db, type_expr, context)
        });

    let signature_parameter = match parameter.kind {
        protocol::ParameterKind::PositionalOnly => Parameter::positional_only(name),
        protocol::ParameterKind::PositionalOrKeyword => Parameter::positional_or_keyword(name?),
        protocol::ParameterKind::VarArgs => {
            Parameter::variadic(name.unwrap_or_else(|| Name::new_static("args")))
        }
        protocol::ParameterKind::KeywordOnly => Parameter::keyword_only(name?),
        protocol::ParameterKind::Kwargs => {
            Parameter::keyword_variadic(name.unwrap_or_else(|| Name::new_static("kwargs")))
        }
    }
    .with_annotated_type(ty);

    match parameter.kind {
        protocol::ParameterKind::PositionalOnly
        | protocol::ParameterKind::PositionalOrKeyword
        | protocol::ParameterKind::KeywordOnly
            if !parameter.required =>
        {
            Some(signature_parameter.with_default_type(ty))
        }
        _ => Some(signature_parameter),
    }
}

fn plugin_callable_type_from_protocol_signature<'db>(
    db: &'db dyn Db,
    signature: &protocol::CallableSignature,
    context: PluginTypeExprContext<'db, '_>,
) -> Type<'db> {
    let parameters = signature
        .parameters
        .iter()
        .filter_map(|parameter| plugin_protocol_parameter_with_context(db, parameter, context))
        .collect::<Vec<_>>();
    let return_ty = plugin_type_expr_to_type_with_context(db, &signature.return_type, context);
    Type::single_callable(
        db,
        Signature::new(
            Parameters::new(parameters, ParametersKind::Standard),
            return_ty,
        ),
    )
}

pub(crate) fn plugin_callable_type_from_protocol_signature_in_class<'db>(
    db: &'db dyn Db,
    signature: &protocol::CallableSignature,
    class: StaticClassLiteral<'db>,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> Type<'db> {
    plugin_callable_type_from_protocol_signature(
        db,
        signature,
        PluginTypeExprContext {
            self_class: Some(class),
            scope: Some(global_scope(db, class.file(db))),
            virtual_types,
            ..PluginTypeExprContext::default()
        },
    )
}

pub(crate) fn plugin_callable_type_from_protocol_signature_with_virtual_types<'db>(
    db: &'db dyn Db,
    signature: &protocol::CallableSignature,
    virtual_types: &[PluginVirtualTypePatch<'db>],
) -> Type<'db> {
    plugin_callable_type_from_protocol_signature(
        db,
        signature,
        PluginTypeExprContext {
            virtual_types,
            ..PluginTypeExprContext::default()
        },
    )
}

fn mock_plugin_execute_call_signature(
    request: &protocol::PluginRequest,
) -> protocol::PluginResponse {
    let protocol::PluginRequest::AdjustCallSignature(_) = request else {
        return protocol::PluginResponse::NoChange;
    };

    // Deterministic mock: replace the callee signature with a single required `value: int`
    // parameter so tests can observe that the adjusted signature drives argument checking.
    protocol::PluginResponse::CallSignaturePatch(protocol::CallSignaturePatch {
        signature: protocol::CallableSignature {
            parameters: vec![protocol::Parameter {
                name: Some("value".to_string()),
                kind: protocol::ParameterKind::PositionalOrKeyword,
                type_expr: Some(protocol::TypeExpr::annotation("int")),
                required: true,
            }],
            return_type: protocol::TypeExpr::annotation("int"),
        },
        diagnostics: Vec::new(),
    })
}

fn mock_plugin_execute_call_return(request: &protocol::PluginRequest) -> protocol::PluginResponse {
    let protocol::PluginRequest::AdjustCallReturn(_) = request else {
        return protocol::PluginResponse::NoChange;
    };

    // Deterministic mock: override the call's return type with `int`.
    protocol::PluginResponse::CallReturnPatch(protocol::CallReturnPatch {
        return_type: protocol::TypeExpr::annotation("int"),
        diagnostics: Vec::new(),
        result_metadata: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use ruff_db::files::system_path_to_file;

    use crate::db::tests::{TestDb, TestDbBuilder, setup_db};

    fn parse_and_display_with_db(db: &TestDb, expression: &str) -> String {
        plugin_type_expr_to_type(db, &protocol::TypeExpr::annotation(expression))
            .display(db)
            .to_string()
    }

    fn parse_and_display(expression: &str) -> String {
        let db = setup_db();
        parse_and_display_with_db(&db, expression)
    }

    #[test]
    fn plugin_type_expr_parses_pep604_and_optional_unions() {
        assert_eq!(parse_and_display("int | None"), "int | None");
        assert_eq!(parse_and_display("typing.Optional[str]"), "str | None");
        assert_eq!(
            parse_and_display("typing.Union[int, str, None]"),
            "int | str | None"
        );
    }

    #[test]
    fn plugin_type_expr_parses_builtin_generic_containers() {
        assert_eq!(parse_and_display("list[int]"), "list[int]");
        assert_eq!(parse_and_display("dict[str, int]"), "dict[str, int]");
        assert_eq!(parse_and_display("tuple[int, bool]"), "tuple[int, bool]");
        assert_eq!(parse_and_display("tuple[str, ...]"), "tuple[str, ...]");
    }

    #[test]
    fn plugin_type_expr_parses_anonymous_typed_dict() {
        let display = parse_and_display(r#"TypedDict({"title": str, "pages": int | None})"#);
        assert!(
            display.contains("TypedDict"),
            "expected a TypedDict display, got {display}"
        );
        assert!(
            display.contains("title"),
            "expected the title field in {display}"
        );
        assert!(
            display.contains("pages"),
            "expected the pages field in {display}"
        );
    }

    #[test]
    fn plugin_type_expr_parses_named_tuple_rows_with_file_scope() -> anyhow::Result<()> {
        let db = TestDbBuilder::new().with_file("/src/app.py", "").build()?;
        let file = system_path_to_file(&db, "/src/app.py").expect("app.py");
        let ty = plugin_type_expr_to_type_in_file(
            &db,
            &protocol::TypeExpr::annotation(
                r#"NamedTuple("BookRow", {"title": str, "pages": int | None})"#,
            ),
            file,
        );
        let display = ty.display(&db).to_string();
        assert!(
            display.contains("BookRow"),
            "expected a named tuple display, got {display}"
        );
        assert_eq!(
            ty.member(&db, "title")
                .place
                .expect_type()
                .display(&db)
                .to_string(),
            "str"
        );
        assert_eq!(
            ty.member(&db, "pages")
                .place
                .expect_type()
                .display(&db)
                .to_string(),
            "int | None"
        );

        Ok(())
    }

    #[test]
    fn plugin_type_expr_resolves_project_virtual_types_and_import_aliases() -> anyhow::Result<()> {
        let db = TestDbBuilder::new()
            .with_file("/src/app.py", "class Book: ...\n")
            .with_file(
                "/src/minidjango.py",
                "from typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\nRow = TypeVar(\"Row\")\n\nclass QuerySet(Generic[T, Row]): ...\n",
            )
            .build()?;
        let file = system_path_to_file(&db, "/src/app.py").expect("app.py");
        let virtual_types = plugin_virtual_type_patches_from_protocol(
            &db,
            vec![
                protocol::VirtualTypeDefinition {
                    name: "minidjango.virtual.app.Book.ValuesRow".to_string(),
                    shape: protocol::VirtualTypeShape::TypedDict {
                        fields: vec![protocol::VirtualTypeField {
                            name: "title".to_string(),
                            type_expr: protocol::TypeExpr::annotation("str"),
                            required: true,
                            read_only: false,
                        }],
                        total: true,
                    },
                    metadata: Default::default(),
                },
                protocol::VirtualTypeDefinition {
                    name: "minidjango.virtual.app.Book.ValuesListRow".to_string(),
                    shape: protocol::VirtualTypeShape::NamedTuple {
                        fields: vec![protocol::VirtualTypeField {
                            name: "title".to_string(),
                            type_expr: protocol::TypeExpr::annotation("str"),
                            required: true,
                            read_only: false,
                        }],
                    },
                    metadata: Default::default(),
                },
            ],
        );

        let query_set = plugin_type_expr_to_type_in_file_with_virtual_types(
            &db,
            &protocol::TypeExpr::annotation(
                "minidjango.QuerySet[app.Book, minidjango.virtual.app.Book.ValuesRow]",
            ),
            file,
            virtual_types.as_ref(),
        );
        let display = query_set.display(&db).to_string();
        assert!(
            display.contains("TypedDict"),
            "expected reusable virtual TypedDict row in {display}"
        );

        let mut aliased = protocol::TypeExpr::annotation("RowAlias");
        aliased.imports.push(protocol::ImportBinding {
            module: "minidjango.virtual.app.Book".to_string(),
            name: "ValuesListRow".to_string(),
            alias: Some("RowAlias".to_string()),
        });
        let named_row = plugin_type_expr_to_type_in_file_with_virtual_types(
            &db,
            &aliased,
            file,
            virtual_types.as_ref(),
        );
        assert_eq!(
            named_row
                .member(&db, "title")
                .place
                .expect_type()
                .display(&db)
                .to_string(),
            "str"
        );

        Ok(())
    }

    #[test]
    fn plugin_type_expr_lowers_virtual_class_shapes() -> anyhow::Result<()> {
        let db = TestDbBuilder::new()
            .with_file("/src/app.py", "class Book: ...\n")
            .with_file(
                "/src/minidjango.py",
                "from typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\n\nclass Manager(Generic[T]):\n    def create(self) -> T: ...\n",
            )
            .build()?;
        let file = system_path_to_file(&db, "/src/app.py").expect("app.py");
        let virtual_types = plugin_virtual_type_patches_from_protocol(
            &db,
            vec![protocol::VirtualTypeDefinition {
                name: "minidjango.virtual.app.Book.Manager".to_string(),
                shape: protocol::VirtualTypeShape::Class {
                    bases: vec![protocol::TypeExpr::expression(
                        "minidjango.Manager[app.Book]",
                    )],
                    members: vec![
                        protocol::MemberPatch {
                            name: "plugin_marker".to_string(),
                            mode: protocol::MemberPatchMode::FillOnMiss,
                            access: protocol::MemberAccessPatch::value(
                                protocol::TypeExpr::annotation("str"),
                            ),
                            read_only: true,
                            diagnostics: Vec::new(),
                        },
                        protocol::MemberPatch {
                            name: "score".to_string(),
                            mode: protocol::MemberPatchMode::FillOnMiss,
                            access: protocol::MemberAccessPatch::Descriptor {
                                class_type: None,
                                instance_get_type: protocol::TypeExpr::annotation("int"),
                                instance_set_type: None,
                            },
                            read_only: true,
                            diagnostics: Vec::new(),
                        },
                    ],
                },
                metadata: Default::default(),
            }],
        );

        let manager = plugin_type_expr_to_type_in_file_with_virtual_types(
            &db,
            &protocol::TypeExpr::annotation("minidjango.virtual.app.Book.Manager"),
            file,
            virtual_types.as_ref(),
        );
        let create = manager
            .member(&db, "create")
            .place
            .expect_type()
            .display(&db)
            .to_string();
        assert!(
            create.contains("Book"),
            "expected inherited generic manager method to preserve Book in {create}"
        );
        assert_eq!(
            manager
                .member(&db, "score")
                .place
                .expect_type()
                .display(&db)
                .to_string(),
            "int"
        );

        Ok(())
    }

    #[test]
    fn plugin_type_expr_parses_inline_class_shapes() -> anyhow::Result<()> {
        let db = TestDbBuilder::new()
            .with_file(
                "/src/minidjango.py",
                r#"
                from typing import Generic, TypeVar

                T = TypeVar("T")
                Row = TypeVar("Row")

                class QuerySet(Generic[T, Row]): ...
                "#,
            )
            .with_file("/src/app.py", "class Book: ...\n")
            .with_file(
                "/src/models.py",
                r#"
                import minidjango

                class Book: ...
                "#,
            )
            .build()?;
        let file = system_path_to_file(&db, "/src/app.py").expect("app.py");

        let annotated = plugin_type_expr_to_type_in_file(
            &db,
            &protocol::TypeExpr::annotation(r#"Class("AnnotatedBook", {"score": int}, app.Book)"#),
            file,
        );
        assert_eq!(annotated.display(&db).to_string(), "AnnotatedBook");
        assert_eq!(
            annotated
                .member(&db, "score")
                .place
                .expect_type()
                .display(&db)
                .to_string(),
            "int"
        );

        let models_file = system_path_to_file(&db, "/src/models.py").expect("models.py");
        let queryset = plugin_type_expr_to_type_in_file(
            &db,
            &protocol::TypeExpr::annotation(
                r#"minidjango.QuerySet[models.Book, Class("MiniDjangoAnnotatedRow", {"score": Literal[1]}, models.Book)]"#,
            ),
            models_file,
        );
        assert_eq!(
            queryset.display(&db).to_string(),
            "QuerySet[Book, MiniDjangoAnnotatedRow]"
        );

        Ok(())
    }

    #[test]
    fn plugin_type_expr_resolves_project_classes_and_generics() -> anyhow::Result<()> {
        let db = TestDbBuilder::new()
            .with_file(
                "/src/app.py",
                "class Book: ...\n",
            )
            .with_file(
                "/src/minidjango.py",
                "from typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\nRow = TypeVar(\"Row\")\n\nclass Manager(Generic[T]): ...\nclass QuerySet(Generic[T, Row]): ...\n",
            )
            .build()?;

        assert_eq!(parse_and_display_with_db(&db, "app.Book"), "Book");
        assert_eq!(
            parse_and_display_with_db(&db, "type[app.Book]"),
            "type[Book]"
        );
        assert_eq!(
            parse_and_display_with_db(&db, "app.Book | None"),
            "Book | None"
        );
        assert_eq!(
            parse_and_display_with_db(&db, "tuple[app.Book, bool]"),
            "tuple[Book, bool]"
        );
        assert_eq!(
            parse_and_display_with_db(&db, "minidjango.Manager[app.Book]"),
            "Manager[Book]"
        );
        assert_eq!(
            parse_and_display_with_db(&db, "minidjango.QuerySet[app.Book, str]"),
            "QuerySet[Book, str]"
        );

        Ok(())
    }

    #[test]
    fn plugin_type_snapshot_round_trips_structural_queryset_rows() -> anyhow::Result<()> {
        let db = TestDbBuilder::new()
            .with_file("/src/app.py", "class Book: ...\n")
            .with_file(
                "/src/minidjango.py",
                "from typing import Generic, TypeVar\n\nT = TypeVar(\"T\")\nRow = TypeVar(\"Row\")\n\nclass QuerySet(Generic[T, Row]): ...\n",
            )
            .build()?;
        let file = system_path_to_file(&db, "/src/app.py").expect("app.py");

        for expression in [
            r#"minidjango.QuerySet[app.Book, TypedDict({"title": str})]"#,
            "minidjango.QuerySet[app.Book, tuple[str, int]]",
        ] {
            let original = plugin_type_expr_to_type_in_file(
                &db,
                &protocol::TypeExpr::annotation(expression),
                file,
            );
            let serialized = plugin_qualified_type_expr_from_type(&db, original);
            assert!(serialized.snapshot.is_some());
            let restored = plugin_type_expr_to_type_in_file(&db, &serialized, file);
            let (_, specialization) = restored
                .class_specialization(&db)
                .expect("specialized QuerySet");
            let row = specialization.types(&db)[1];

            if expression.contains("TypedDict") {
                let Type::TypedDict(row) = row else {
                    panic!("expected TypedDict row, got {}", row.display(&db));
                };
                assert_eq!(
                    row.item(&db, "title")
                        .expect("title field")
                        .declared_ty
                        .display(&db)
                        .to_string(),
                    "str"
                );
            } else {
                let tuple = row.tuple_instance_spec(&db).expect("tuple row");
                assert_eq!(
                    tuple
                        .iter_all_elements()
                        .map(|element| element.display(&db).to_string())
                        .collect::<Vec<_>>(),
                    ["str", "int"]
                );
            }
        }

        let named_original = plugin_type_expr_to_type_in_file(
            &db,
            &protocol::TypeExpr::annotation(
                r#"minidjango.QuerySet[app.Book, NamedTuple("Row", {"title": str, "pages": int})]"#,
            ),
            file,
        );
        let named_serialized = plugin_qualified_type_expr_from_type(&db, named_original);
        let named_restored = plugin_type_expr_to_type_in_file(&db, &named_serialized, file);
        let (_, specialization) = named_restored
            .class_specialization(&db)
            .expect("specialized QuerySet");
        let named_row = specialization.types(&db)[1];
        assert_eq!(
            named_row
                .member(&db, "pages")
                .place
                .expect_type()
                .display(&db)
                .to_string(),
            "int"
        );

        Ok(())
    }

    #[test]
    fn plugin_type_expr_parses_literal_scalars() {
        assert_eq!(
            parse_and_display("typing.Literal['draft']"),
            "Literal[\"draft\"]"
        );
        assert_eq!(
            parse_and_display("typing.Literal[1, True]"),
            "Literal[1, True]"
        );
        assert_eq!(
            parse_and_display("typing.Literal[-1, +2]"),
            "Literal[-1, 2]"
        );
        assert_eq!(
            parse_and_display("typing.Literal['a,b', \"x|y\"]"),
            "Literal[\"a,b\", \"x|y\"]"
        );
        assert_eq!(
            parse_and_display("typing.Literal[b'a,b']"),
            "Literal[b\"a,b\"]"
        );
    }
}
