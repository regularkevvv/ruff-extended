//! Evaluation histories for plugin-owned object member facts.
//!
//! Histories share prefixes and use the same snapshots as ordinary bindings. Semantic inference
//! interprets calls; the index records only evaluation order, aliases, and control-flow joins.

use ruff_python_ast::name::Name;
use rustc_hash::FxHashMap;

use crate::ExpressionNodeKey;
use crate::expression::Expression;
use crate::reachability_constraints::ScopedReachabilityConstraintId;

#[derive(Clone, Copy, Debug)]
pub(super) enum ObjectStateTracking {
    Disabled,
    Enabled,
}

impl From<bool> for ObjectStateTracking {
    fn from(enabled: bool) -> Self {
        if enabled {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }
}

impl ObjectStateTracking {
    pub(super) fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }
}

pub type ObjectStateId = usize;

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub enum ObjectReference<'db> {
    Name(Name),
    Call(Expression<'db>),
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub enum ObjectStateEvent<'db> {
    Call(Expression<'db>),
    Operation(Expression<'db>),
    Bind {
        name: Name,
        value: ObjectReference<'db>,
    },
    /// Arbitrary writes, implicit user-code execution, and suspension discard member facts.
    Invalidate,
    /// Repeated evaluation can change identities as well as member values.
    Reset,
}

#[derive(Clone, Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub enum ObjectStateNode<'db> {
    Step {
        previous: Option<ObjectStateId>,
        event: ObjectStateEvent<'db>,
    },
    Join {
        left: Option<ObjectStateId>,
        right: Option<ObjectStateId>,
        left_reachability: ScopedReachabilityConstraintId,
        right_reachability: ScopedReachabilityConstraintId,
    },
}

#[derive(Debug, PartialEq, Eq, get_size2::GetSize, salsa::SalsaValue)]
pub struct ObjectStateFlow<'db> {
    pub nodes: Box<[ObjectStateNode<'db>]>,
    reads: Box<[(ExpressionNodeKey, Option<ObjectStateId>)]>,
    calls: Box<[(ExpressionNodeKey, Expression<'db>)]>,
}

impl<'db> ObjectStateFlow<'db> {
    pub fn call(&self, key: ExpressionNodeKey) -> Option<Expression<'db>> {
        self.calls
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .map(|i| self.calls[i].1)
    }
    pub fn before(&self, key: ExpressionNodeKey) -> Option<ObjectStateId> {
        self.reads
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .and_then(|i| self.reads[i].1)
    }
}

#[derive(Debug, Default)]
pub(super) struct ObjectStateFlowBuilder<'db> {
    nodes: Vec<ObjectStateNode<'db>>,
    calls: FxHashMap<ExpressionNodeKey, Expression<'db>>,
    reads: FxHashMap<ExpressionNodeKey, Option<ObjectStateId>>,
    pub(super) current: Option<ObjectStateId>,
}

impl<'db> ObjectStateFlowBuilder<'db> {
    pub(super) fn call(&self, key: ExpressionNodeKey) -> Option<Expression<'db>> {
        self.calls.get(&key).copied()
    }

    pub(super) fn record_call(&mut self, key: ExpressionNodeKey, expression: Expression<'db>) {
        self.calls.insert(key, expression);
        self.record(ObjectStateEvent::Call(expression));
    }

    pub(super) fn record(&mut self, event: ObjectStateEvent<'db>) {
        let id = self.nodes.len();
        self.nodes.push(ObjectStateNode::Step {
            previous: self.current,
            event,
        });
        self.current = Some(id);
    }

    pub(super) fn read(&mut self, key: ExpressionNodeKey) {
        self.reads.insert(key, self.current);
    }

    pub(super) fn join(
        &mut self,
        right: Option<ObjectStateId>,
        left_reachability: ScopedReachabilityConstraintId,
        right_reachability: ScopedReachabilityConstraintId,
    ) {
        if self.current == right {
            return;
        }
        let id = self.nodes.len();
        self.nodes.push(ObjectStateNode::Join {
            left: self.current,
            right,
            left_reachability,
            right_reachability,
        });
        self.current = Some(id);
    }

    pub(super) fn finish(self) -> ObjectStateFlow<'db> {
        let mut reads: Vec<_> = self.reads.into_iter().collect();
        reads.sort_unstable_by_key(|(key, _)| *key);
        let mut calls: Vec<_> = self.calls.into_iter().collect();
        calls.sort_unstable_by_key(|(key, _)| *key);
        ObjectStateFlow {
            calls: calls.into_boxed_slice(),
            nodes: self.nodes.into_boxed_slice(),
            reads: reads.into_boxed_slice(),
        }
    }
}
