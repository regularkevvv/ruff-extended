//! Flow-sensitive member facts supplied by semantic plugins.

use std::collections::BTreeMap;

use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::{self as ast, ExprContext};
use rustc_hash::FxHashMap;
use ty_python_core::ast_node_ref::AstNodeRef;
use ty_python_core::definition::DefinitionKind;
use ty_python_core::expression::{Expression, ExpressionKind};
use ty_python_core::object_state::{
    ObjectReference, ObjectStateEvent, ObjectStateId, ObjectStateNode,
};
use ty_python_core::scope::ScopeId;
use ty_python_core::{ExpressionNodeKey, place_table, semantic_index, use_def_map};

use super::TypeContext;
use super::call::CallArguments;
use super::infer::infer_same_file_expression_type;
use super::plugin::{
    PluginRuntimeDiagnostic, plugin_adjusted_call_state, plugin_claims_call_state,
};
use super::{KnownFunction, Type, UnionType};
use crate::reachability::evaluate_reachability;
use crate::{Db, ProgramEnvironment};

#[derive(Debug, Clone, PartialEq, Eq, Default, salsa::SalsaValue, get_size2::GetSize)]
pub(crate) struct CallStateEffects<'db> {
    pub receiver_members: BTreeMap<Name, Type<'db>>,
    pub result_members: BTreeMap<Name, Type<'db>>,
    pub fresh_result: bool,
    pub has_receiver: bool,
    pub preserves_other_objects: bool,
}

/// The state hook has its own memo so each call site's effects are shared by all later reads.
#[salsa::tracked(returns(ref), cycle_initial=|_, _, _| Ok(None), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn call_state_effects<'db>(
    db: &'db dyn Db,
    expression: Expression<'db>,
) -> Result<Option<CallStateEffects<'db>>, PluginRuntimeDiagnostic> {
    let program_file = expression.program_file(db);
    let module = parsed_module(db, program_file.python_file(db)).load(db);
    let ast::Expr::Call(call) = expression.node_ref(db).node(&module) else {
        return Ok(None);
    };
    let index = semantic_index(db, program_file);
    let infer = |expr: &ast::Expr| {
        let expression = index.try_expression(expr).unwrap_or_else(|| {
            Expression::new(
                db,
                expression.scope(db),
                AstNodeRef::new(&module, expr),
                None,
                ExpressionKind::Normal,
            )
        });
        infer_same_file_expression_type(db, expression, TypeContext::default())
    };
    let callable = infer(&call.func);
    // These checker intrinsics inspect types without executing arbitrary user code.
    if callable
        .as_function_literal()
        .and_then(|function| function.known(db))
        .is_some_and(|known| matches!(known, KnownFunction::RevealType | KnownFunction::AssertType))
    {
        return Ok(Some(CallStateEffects {
            preserves_other_objects: true,
            ..CallStateEffects::default()
        }));
    }
    if !plugin_claims_call_state(db, program_file.file(db), callable) {
        return Ok(None);
    }
    let mut arguments = CallArguments::from_arguments(&call.arguments, |_, value| infer(value));
    for (index, argument) in call.arguments.iter_source_order().enumerate() {
        let value = match argument {
            ast::ArgOrKeyword::Arg(ast::Expr::Starred(starred)) => &*starred.value,
            ast::ArgOrKeyword::Arg(value) => value,
            ast::ArgOrKeyword::Keyword(keyword) => &keyword.value,
        };
        arguments.insert_type(index, TypeContext::default(), infer(value));
    }
    plugin_adjusted_call_state(
        db,
        program_file.file(db),
        callable,
        &call.arguments,
        &arguments,
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, salsa::SalsaValue, get_size2::GetSize)]
enum ObjectIdentity<'db> {
    Fresh(Expression<'db>),
    External(Name, Option<ObjectStateId>),
    ExternalCall(Expression<'db>),
}

#[derive(Debug, Clone, PartialEq, Eq, salsa::SalsaValue, get_size2::GetSize)]
struct MemberFact<'db> {
    ty: Type<'db>,
    /// At least one reaching path has no plugin fact and requires the ordinary member type.
    include_default: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, salsa::SalsaValue, get_size2::GetSize)]
struct ObjectState<'db> {
    names: FxHashMap<Name, ObjectIdentity<'db>>,
    facts: FxHashMap<(ObjectIdentity<'db>, Name), MemberFact<'db>>,
}

impl<'db> ObjectState<'db> {
    fn reference(
        &self,
        db: &'db dyn Db,
        reference: &ObjectReference<'db>,
    ) -> Option<ObjectIdentity<'db>> {
        match reference {
            ObjectReference::Name(name) => Some(
                self.names
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| ObjectIdentity::External(name.clone(), None)),
            ),
            ObjectReference::Call(call) => Some(
                if call_state_effects(db, *call)
                    .as_ref()
                    .ok()
                    .and_then(|effects| effects.as_ref())
                    .is_some_and(|effects| effects.fresh_result)
                {
                    ObjectIdentity::Fresh(*call)
                } else {
                    ObjectIdentity::ExternalCall(*call)
                },
            ),
            ObjectReference::Unknown => None,
        }
    }

    #[expect(
        clippy::iter_over_hash_type,
        reason = "Each object/member pair is joined independently."
    )]
    fn join(
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        mut left: Self,
        right: Self,
        id: ObjectStateId,
    ) -> Self {
        let mut names: Vec<_> = left
            .names
            .keys()
            .chain(right.names.keys())
            .cloned()
            .collect();
        names.sort_unstable();
        names.dedup();
        let mut pairs = FxHashMap::default();
        let mut joined_names = Vec::new();
        let mut joined_facts = Vec::new();
        for name in names {
            let reference = ObjectReference::Name(name.clone());
            let (Some(left_id), Some(right_id)) = (
                left.reference(db, &reference),
                right.reference(db, &reference),
            ) else {
                continue;
            };
            if left_id == right_id {
                continue;
            }
            let identity = pairs
                .entry((left_id.clone(), right_id.clone()))
                .or_insert_with(|| ObjectIdentity::External(name.clone(), Some(id)))
                .clone();
            let mut members: Vec<_> = left
                .facts
                .keys()
                .filter(|(owner, _)| owner == &left_id)
                .chain(right.facts.keys().filter(|(owner, _)| owner == &right_id))
                .map(|(_, name)| name.clone())
                .collect();
            members.sort_unstable();
            members.dedup();
            for member in members {
                let first = left.facts.get(&(left_id.clone(), member.clone()));
                let second = right.facts.get(&(right_id.clone(), member.clone()));
                let fact = MemberFact {
                    ty: UnionType::from_elements(
                        db,
                        env,
                        [
                            first.map_or(Type::Never, |fact| fact.ty),
                            second.map_or(Type::Never, |fact| fact.ty),
                        ],
                    ),
                    include_default: first.is_none_or(|fact| fact.include_default)
                        || second.is_none_or(|fact| fact.include_default),
                };
                joined_facts.push(((identity.clone(), member), fact));
            }
            joined_names.push((name, identity));
        }
        for (key, fact) in &mut left.facts {
            if let Some(other) = right.facts.get(key) {
                fact.ty = UnionType::from_elements(db, env, [fact.ty, other.ty]);
                fact.include_default |= other.include_default;
            } else {
                fact.include_default = true;
            }
        }
        for (key, mut fact) in right.facts {
            left.facts.entry(key).or_insert_with(|| {
                fact.include_default = true;
                fact
            });
        }
        left.names.extend(joined_names);
        left.facts.extend(joined_facts);
        left
    }

    #[expect(
        clippy::iter_over_hash_type,
        reason = "Invalidation replaces each fact independently."
    )]
    fn invalidate(&mut self) {
        for fact in self.facts.values_mut() {
            fact.ty = Type::Never;
            fact.include_default = true;
        }
    }

    fn invalidate_execution(&mut self, db: &'db dyn Db, scope: ScopeId<'db>, id: ObjectStateId) {
        self.invalidate();
        // Unknown code can rebind globals and closure cells, but cannot rebind ordinary locals.
        for name in externally_mutable_names(db, scope) {
            let Some(previous) = self.names.get(name).cloned() else {
                continue;
            };
            let identity = ObjectIdentity::External(name.clone(), Some(id));
            let members: Vec<_> = self
                .facts
                .iter()
                .filter(|((owner, _), _)| *owner == previous)
                .map(|((_, member), fact)| ((identity.clone(), member.clone()), fact.clone()))
                .collect();
            self.facts.extend(members);
            self.names.insert(name.clone(), identity);
        }
    }

    #[expect(
        clippy::iter_over_hash_type,
        reason = "Receiver invalidation replaces each fact independently."
    )]
    fn call(&mut self, db: &'db dyn Db, expression: Expression<'db>, id: ObjectStateId) {
        let module = parsed_module(db, expression.program_file(db).python_file(db)).load(db);
        let ast::Expr::Call(call) = expression.node_ref(db).node(&module) else {
            self.invalidate_execution(db, expression.scope(db), id);
            return;
        };
        let receiver = call.func.as_attribute_expr().and_then(|attr| {
            let reference = object_reference(db, expression.scope(db), &attr.value)?;
            // Python evaluates the bound method before its arguments. An argument may rebind
            // the name used to obtain that method without changing the captured receiver.
            let map = use_def_map(db, expression.scope(db));
            let prefix = map
                .object_state()
                .and_then(|flow| flow.before(call.func.as_ref().into()));
            prefix
                .and_then(|id| state_at(db, expression.scope(db), id).reference(db, &reference))
                .or_else(|| self.reference(db, &reference))
        });
        let Ok(Some(effects)) = call_state_effects(db, expression) else {
            self.invalidate_execution(db, expression.scope(db), id);
            return;
        };
        if !effects.preserves_other_objects || (effects.has_receiver && receiver.is_none()) {
            self.invalidate_execution(db, expression.scope(db), id);
        } else if let Some(receiver) = &receiver {
            // External names may denote the same object even without an explicit assignment.
            for ((identity, _), fact) in &mut self.facts {
                if identity == receiver
                    || !matches!(receiver, ObjectIdentity::Fresh(_))
                    || !matches!(identity, ObjectIdentity::Fresh(_))
                {
                    fact.ty = Type::Never;
                    fact.include_default = true;
                }
            }
        }
        if let Some(receiver) = receiver {
            for (name, ty) in &effects.receiver_members {
                self.facts.insert(
                    (receiver.clone(), name.clone()),
                    MemberFact {
                        ty: *ty,
                        include_default: false,
                    },
                );
            }
        }
        if effects.fresh_result {
            for (name, ty) in &effects.result_members {
                self.facts.insert(
                    (ObjectIdentity::Fresh(expression), name.clone()),
                    MemberFact {
                        ty: *ty,
                        include_default: false,
                    },
                );
            }
        }
    }
}

#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn externally_mutable_names<'db>(db: &'db dyn Db, scope: ScopeId<'db>) -> Box<[Name]> {
    let places = place_table(db, scope);
    let map = use_def_map(db, scope);
    places
        .symbols()
        .filter(|symbol| {
            scope.scope(db).kind().is_module()
                || !symbol.is_local()
                || places.symbol_id(symbol.name()).is_some_and(|id| {
                    map.reachable_symbol_bindings(id).any(|binding| {
                        binding.binding.definition().is_some_and(|definition| {
                            matches!(definition.kind(db), DefinitionKind::NestedBindings(_))
                        })
                    })
                })
        })
        .map(|symbol| symbol.name().clone())
        .collect()
}

fn object_reference<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    expression: &ast::Expr,
) -> Option<ObjectReference<'db>> {
    match expression {
        ast::Expr::Name(name) => Some(ObjectReference::Name(name.id.clone())),
        ast::Expr::Call(_) => use_def_map(db, scope)
            .object_state()
            .and_then(|flow| flow.call(expression.into()))
            .map(ObjectReference::Call),
        _ => None,
    }
}

/// Builtin union syntax in an `assert_type` argument does not invoke user-defined operators.
#[salsa::tracked(cycle_initial=|_, _, _| false)]
fn is_builtin_type_union<'db>(db: &'db dyn Db, expression: Expression<'db>) -> bool {
    fn is_type(
        db: &dyn Db,
        expression: Expression<'_>,
        module: &ruff_db::parsed::ParsedModuleRef,
        value: &ast::Expr,
    ) -> bool {
        if let ast::Expr::BinOp(binary) = value {
            return binary.op == ast::Operator::BitOr
                && is_type(db, expression, module, &binary.left)
                && is_type(db, expression, module, &binary.right);
        }
        let ty = infer_same_file_expression_type(
            db,
            Expression::new(
                db,
                expression.scope(db),
                AstNodeRef::new(module, value),
                None,
                ExpressionKind::Normal,
            ),
            TypeContext::default(),
        );
        ty.is_none(db)
            || ty
                .as_class_literal()
                .is_some_and(|class| class.known(db).is_some())
    }
    let module = parsed_module(db, expression.program_file(db).python_file(db)).load(db);
    let ast::Expr::BinOp(binary) = expression.node_ref(db).node(&module) else {
        return false;
    };
    binary.op == ast::Operator::BitOr
        && is_type(db, expression, &module, &binary.left)
        && is_type(db, expression, &module, &binary.right)
}

/// Straight-line prefixes are evaluated iteratively, avoiding a Salsa stack frame per assignment.
#[salsa::tracked(returns(ref), cycle_initial=|_, _, _, _| ObjectState::default(), heap_size=ruff_memory_usage::heap_size)]
fn state_at<'db>(db: &'db dyn Db, scope: ScopeId<'db>, id: ObjectStateId) -> ObjectState<'db> {
    let map = use_def_map(db, scope);
    let Some(flow) = map.object_state() else {
        return ObjectState::default();
    };
    let previous = |id: Option<ObjectStateId>| {
        id.map_or_else(ObjectState::default, |id| state_at(db, scope, id).clone())
    };
    let mut steps = Vec::new();
    let mut cursor = Some(id);
    let mut state = ObjectState::default();
    while let Some(id) = cursor {
        match &flow.nodes[id] {
            ObjectStateNode::Step {
                event: ObjectStateEvent::Reset,
                ..
            } => {
                break;
            }
            ObjectStateNode::Step { previous, event } => {
                steps.push((id, event));
                cursor = *previous;
            }
            ObjectStateNode::Join {
                left,
                right,
                left_reachability,
                right_reachability,
            } => {
                state = if evaluate_reachability(db, map, *left_reachability).is_always_false() {
                    previous(*right)
                } else if evaluate_reachability(db, map, *right_reachability).is_always_false() {
                    previous(*left)
                } else {
                    ObjectState::join(
                        db,
                        &ProgramEnvironment::from_scope(scope),
                        previous(*left),
                        previous(*right),
                        id,
                    )
                };
                break;
            }
        }
    }
    for (id, event) in steps.into_iter().rev() {
        match event {
            ObjectStateEvent::Call(expression) => state.call(db, *expression, id),
            ObjectStateEvent::Bind { name, value } => {
                let identity = state
                    .reference(db, value)
                    .unwrap_or_else(|| ObjectIdentity::External(name.clone(), Some(id)));
                state.names.insert(name.clone(), identity);
            }
            ObjectStateEvent::Operation(expression) => {
                if !is_builtin_type_union(db, *expression) {
                    state.invalidate_execution(db, scope, id);
                }
            }
            ObjectStateEvent::Invalidate => state.invalidate_execution(db, scope, id),
            ObjectStateEvent::Reset => {}
        }
    }
    state.names.shrink_to_fit();
    state.facts.shrink_to_fit();
    state
}

pub(crate) fn refined_member_type<'db>(
    db: &'db dyn Db,
    scope: ScopeId<'db>,
    attribute: &ast::ExprAttribute,
    default: Type<'db>,
    owner: Type<'db>,
) -> Type<'db> {
    if attribute.ctx != ExprContext::Load {
        return default;
    }
    let map = use_def_map(db, scope);
    let Some(id) = map
        .object_state()
        .and_then(|flow| flow.before(ExpressionNodeKey::from(ast::ExprRef::Attribute(attribute))))
    else {
        return default;
    };
    let Some(reference) = object_reference(db, scope, &attribute.value) else {
        return default;
    };
    let state = state_at(db, scope, id);
    let Some(identity) = state.reference(db, &reference) else {
        return default;
    };
    let Some(fact) = state
        .facts
        .get(&(identity, Name::from(attribute.attr.as_str())))
    else {
        return default;
    };
    if fact.include_default {
        let declared = owner
            .member(
                db,
                &ProgramEnvironment::from_scope(scope),
                attribute.attr.as_str(),
            )
            .place
            .raw_type()
            .unwrap_or(default);
        UnionType::from_elements(
            db,
            &ProgramEnvironment::from_scope(scope),
            [fact.ty, declared],
        )
    } else {
        fact.ty
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fmt::Write as _;

    use ruff_db::files::system_path_to_file;
    use ruff_db::system::DbWithWritableSystem as _;
    use ty_plugin_protocol as protocol;
    use ty_python_core::program::{
        SemanticPlugin, SemanticPluginEnvironment, SemanticPluginMethodClaim,
        SemanticPluginRuntime, SemanticPlugins,
    };

    use crate::Db as _;
    use crate::db::tests::{TestDb, TestDbBuilder};

    fn fixture() -> anyhow::Result<TestDb> {
        let mut db = TestDbBuilder::new().build()?;
        db.write_dedented(
            "/src/records.py",
            r#"
            class Record:
                key: int | None
                pk: int | None
                def __init__(self, key: int | None = None) -> None: ...
                def set_key(self, *args: object) -> None: ...
                def clear_key(self) -> None: ...
                def unclaimed(self) -> None: ...

            def load() -> Record: ...
            def shared() -> Record: ...
            def touch() -> None: ...
            def unknown(value: Record) -> None: ...
            def identity(value: Record) -> Record: ...

            class Child(Record): ...

            class Override(Record):
                def set_key(self) -> None: ...
        "#,
        )?;
        db.register_semantic_plugin_executor("state".to_string(), |request| {
            let protocol::PluginRequest::AdjustCallState(call) = request else {
                return Ok(protocol::PluginResponse::NoChange);
            };
            let members = |ty: &str| {
                BTreeMap::from([
                    ("key".to_string(), protocol::TypeExpr::annotation(ty)),
                    ("pk".to_string(), protocol::TypeExpr::annotation(ty)),
                ])
            };
            let (receiver_members, result_members, fresh_result) =
                match call.callee.expression.as_str() {
                    "records.Record" | "records.Child" => {
                        let ty = if call.arguments.iter().any(|argument| {
                            argument
                                .type_expr
                                .as_ref()
                                .is_some_and(|ty| ty.expression != "None")
                        }) {
                            "int"
                        } else {
                            "None"
                        };
                        (BTreeMap::new(), members(ty), true)
                    }
                    "records.load" => (BTreeMap::new(), members("int"), true),
                    "records.shared" => {
                        return Ok(protocol::PluginResponse::CallStatePatch(
                            protocol::CallStatePatch {
                                result_members: members("int"),
                                ..Default::default()
                            },
                        ));
                    }
                    "records.touch" => {
                        return Ok(protocol::PluginResponse::CallStatePatch(
                            protocol::CallStatePatch::default(),
                        ));
                    }
                    "records.Record.set_key" => (members("int"), BTreeMap::new(), false),
                    "records.Record.clear_key" => (members("None"), BTreeMap::new(), false),
                    _ => return Ok(protocol::PluginResponse::NoChange),
                };
            Ok(protocol::PluginResponse::CallStatePatch(
                protocol::CallStatePatch {
                    receiver_members,
                    result_members,
                    fresh_result,
                    preserves_other_objects: true,
                },
            ))
        });
        let plugin = SemanticPlugin::new(
            "state",
            SemanticPluginRuntime::InProcess,
            Vec::<String>::new(),
            Vec::new(),
            Vec::new(),
            Vec::<String>::new(),
            Vec::<String>::new(),
        )
        .with_call_state_claims(
            vec![
                "records.load".to_string(),
                "records.shared".to_string(),
                "records.touch".to_string(),
            ],
            vec![
                SemanticPluginMethodClaim::on_subclass_of("records.Record", "set_key"),
                SemanticPluginMethodClaim::on_subclass_of("records.Record", "clear_key"),
            ],
        );
        let plugin = plugin.with_call_state_constructor_claims(
            vec!["records.Record".to_string()],
            vec!["records.Record".to_string()],
        );
        SemanticPlugins::init_or_update(&mut db, SemanticPluginEnvironment::new(1, vec![plugin]));
        Ok(db)
    }

    fn check(source: &str) -> anyhow::Result<()> {
        let mut db = fixture()?;
        db.write_dedented("/src/main.py", source)?;
        let file = system_path_to_file(&db, "/src/main.py")?;
        let diagnostics = db.check_file(file);
        assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        Ok(())
    }

    #[test]
    fn construction_loading_and_receiver_changes() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Record, load
            record = Record()
            assert_type(record.key, None)
            assert_type(record.pk, None)
            assert_type(record.set_key(), None)
            assert_type(record.key, int)
            assert_type(record.pk, int)
            record.clear_key()
            assert_type(record.key, None)
            explicit = Record(key=123)
            assert_type(explicit.key, int)
            loaded = load()
            assert_type(loaded.key, int)
            assert_type(load().key, int)
            assert_type(explicit.key, int)
        "#,
        )
    }

    #[test]
    fn subclass_constructor_state_keeps_class_identity() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Child
            child = Child()
            assert_type(child, Child)
            assert_type(child.key, None)
            assert_type(Child(key=123).pk, int)
            class Unrelated:
                key: int | None
            other = Unrelated()
            assert_type(other.key, int | None)
        "#,
        )
    }

    #[test]
    fn branch_joins_and_early_returns() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Record
            def conditional(condition: bool):
                record = Record()
                if condition:
                    record.set_key()
                    assert_type(record.key, int)
                assert_type(record.key, int | None)
            def both(condition: bool):
                record = Record()
                if condition:
                    record.set_key()
                else:
                    record.set_key()
                assert_type(record.key, int)
            def terminal(condition: bool):
                record = Record()
                if condition:
                    return
                record.set_key()
                assert_type(record.key, int)
            def constant():
                record = Record()
                if False:
                    record.set_key()
                assert_type(record.key, None)
        "#,
        )
    }

    #[test]
    fn aliases_and_rebinding() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Record, load
            record = load()
            alias = record
            second = alias
            alias.clear_key()
            assert_type(record.key, None)
            assert_type(second.pk, None)
            alias = Record(key=9)
            alias.clear_key()
            assert_type(record.key, None)
            record.set_key()
            assert_type(second.key, int)
            assert_type(alias.key, None)
            first = second = Record()
            second.set_key()
            assert_type(first.pk, int)
        "#,
        )
    }

    #[test]
    fn unknown_calls_and_overrides() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Record, Child, Override, unknown, load, identity
            record = load()
            unknown(record)
            assert_type(record.key, int | None)
            record.set_key()
            record.unclaimed()
            assert_type(record.key, int | None)
            child = Child()
            child.set_key()
            assert_type(child.key, int)
            overridden = Override()
            overridden.set_key()
            assert_type(overridden.key, int | None)
            record = load()
            alias = identity(record)
            record.set_key()
            alias.clear_key()
            assert_type(record.key, int | None)
            def possibly_same(first: Record, second: Record):
                first.set_key()
                second.clear_key()
                assert_type(first.key, int | None)
                assert_type(second.key, None)
        "#,
        )
    }

    #[test]
    fn conditional_aliases_and_short_circuit_calls() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Record
            def ambiguous(condition: bool):
                first = Record(key=1)
                second = Record(key=2)
                if condition:
                    alias = first
                else:
                    alias = second
                alias.clear_key()
                assert_type(first.key, int | None)
                assert_type(second.key, int | None)
                assert_type(alias.key, None)
            def short_circuit(condition: bool):
                record = Record()
                condition and record.set_key()
                assert_type(record.key, int | None)
        "#,
        )
    }

    #[test]
    fn writes_loops_and_exception_paths() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Record, load
            record = load()
            record.key = None
            assert_type(record.pk, int | None)
            def loop(condition: bool):
                record = Record()
                while condition:
                    assert_type(record.key, int | None)
                    record.set_key()
                    assert_type(record.key, int)
                assert_type(record.key, int | None)
            def exception():
                record = load()
                try:
                    record.clear_key()
                except Exception:
                    assert_type(record.key, int | None)
                assert_type(record.key, int | None)
        "#,
        )
    }
    #[test]
    fn invalidation_discards_member_narrowing() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Record, load, unknown
            record = load()
            if record.key is not None:
                unknown(record)
                assert_type(record.key, int | None)
        "#,
        )
    }
    #[test]
    fn branch_allocations_keep_member_facts_and_shared_aliases() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import load
            def allocate(condition: bool):
                if condition:
                    record = load()
                    alias = record
                else:
                    record = load()
                    alias = record
                assert_type(record.key, int)
                alias.clear_key()
                assert_type(record.key, None)
        "#,
        )
    }
    #[test]
    fn suspension_and_context_managers_invalidate_facts() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Record, load
            async def pause() -> None: ...
            async def suspend():
                record = load()
                await pause()
                assert_type(record.key, int | None)
            def generator():
                record = load()
                yield record
                assert_type(record.key, int | None)
            class Context:
                def __enter__(self) -> None: ...
                def __exit__(self, exc_type, exc, traceback) -> None: ...
            def context(manager: Context):
                record = load()
                with manager:
                    assert_type(record.key, int | None)
                    record.set_key()
                    assert_type(record.key, int)
                assert_type(record.key, int | None)
        "#,
        )
    }

    #[test]
    fn long_straight_line_history() -> anyhow::Result<()> {
        let mut source = String::from(
            "from typing_extensions import assert_type\nfrom records import Record\nrecord = Record()\n",
        );
        for index in 0..2000 {
            writeln!(source, "value_{index} = {index}")?;
        }
        source.push_str(
            "assert_type(record.key, None)\nrecord.set_key()\nassert_type(record.key, int)\n",
        );
        check(&source)
    }

    #[test]
    fn disabled_state_hook_leaves_declared_members() -> anyhow::Result<()> {
        let mut db = fixture()?;
        SemanticPlugins::init_or_update(&mut db, SemanticPluginEnvironment::default());
        db.write_dedented(
            "/src/main.py",
            r#"
            from typing_extensions import assert_type
            from records import Record
            record = Record()
            record.set_key()
            assert_type(record.key, int | None)
        "#,
        )?;
        let file = system_path_to_file(&db, "/src/main.py")?;
        assert!(db.check_file(file).is_empty());
        Ok(())
    }
    #[test]
    fn default_invalidation_and_nonfresh_results() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import load, shared, touch
            record = load()
            touch()
            assert_type(record.key, int | None)
            result = shared()
            assert_type(result.key, int | None)
        "#,
        )
    }

    #[test]
    fn state_changes_after_an_edit() -> anyhow::Result<()> {
        let mut db = fixture()?;
        for (constructor, expected) in [
            ("Record()", "None"),
            ("Record(key=1)", "int"),
            ("Record()", "None"),
        ] {
            db.write_file("/src/main.py", format!("from typing_extensions import assert_type\nfrom records import Record\nrecord = {constructor}\nassert_type(record.key, {expected})\n"))?;
            let file = system_path_to_file(&db, "/src/main.py")?;
            assert!(db.check_file(file).is_empty());
        }
        Ok(())
    }

    #[test]
    fn state_hook_failure_reports_and_discards_facts() -> anyhow::Result<()> {
        let mut db = fixture()?;
        db.register_semantic_plugin_executor("state".to_string(), |_| {
            Err(crate::SemanticPluginRuntimeError::new(
                "state hook failed",
                "check plugin",
            ))
        });
        db.write_file(
            "/src/main.py",
            "from records import Record\nrecord = Record()\nrecord.set_key()\n",
        )?;
        let file = system_path_to_file(&db, "/src/main.py")?;
        let diagnostics = db.check_file(file);
        assert!(
            diagnostics.iter().any(|diagnostic| diagnostic.id()
                == ruff_db::diagnostic::DiagnosticId::PluginConfiguration),
            "{diagnostics:#?}"
        );
        Ok(())
    }
    #[test]
    fn indirect_receivers_and_unknown_results_are_conservative() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Record, load, identity
            class Holder:
                value: Record
            def indirect(holder: Holder):
                record = load()
                holder.value = record
                record.set_key()
                holder.value.clear_key()
                assert_type(record.key, int | None)
            record = load()
            action = record.clear_key
            action()
            assert_type(record.key, int | None)
            first = second = identity(record)
            first.set_key()
            second.clear_key()
            assert_type(first.key, None)
            def possible_alias():
                record = load()
                result = identity(record)
                result.clear_key()
                record.set_key()
                assert_type(result.key, int | None)
        "#,
        )
    }
    #[test]
    fn arguments_can_rebind_the_receiver_name() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Record
            record = Record()
            alias = record
            other = Record()
            record.set_key(record := other)
            assert_type(alias.key, int)
            assert_type(record.key, None)
        "#,
        )
    }

    #[test]
    fn unknown_calls_can_rebind_globals_and_closure_cells() -> anyhow::Result<()> {
        check(
            r#"
            from typing_extensions import assert_type
            from records import Record, load, unknown
            record = load()
            alias = record
            unknown(record)
            record.set_key()
            assert_type(alias.key, int | None)
            def captured():
                record = load()
                alias = record
                def replace():
                    nonlocal record
                    record = Record()
                replace()
                record.set_key()
                assert_type(alias.key, int | None)
            def local():
                record = load()
                alias = record
                unknown(record)
                record.set_key()
                assert_type(alias.key, int)
        "#,
        )
    }
}
