//! Conversion of `moraine-atom` ASTs into the resolver-local string AST.
//!
//! Each source repository and installed store interns identifiers against its
//! own interner. These helpers resolve those symbols to owned strings once, so
//! the encoder and provider operate in a single canonical namespace.

use moraine_atom::{Atom, Blocker, DepSpec, Operator, SlotOp, UseDepKind};
use moraine_common::Interner;

use crate::depnode::{BlockerKind, DepNode, NormAtom, Op, SlotOpKind, UseReq, UseReqKind};

/// Convert a `moraine-atom` `DepSpec` into a resolver-local `DepNode`.
pub fn normalize_depspec(spec: &DepSpec, interner: &Interner) -> DepNode {
    match spec {
        DepSpec::AllOf(children) => DepNode::AllOf(
            children
                .iter()
                .map(|c| normalize_depspec(c, interner))
                .collect(),
        ),
        DepSpec::AnyOf(children) => DepNode::AnyOf(
            children
                .iter()
                .map(|c| normalize_depspec(c, interner))
                .collect(),
        ),
        DepSpec::Conditional { flag, sense, body } => DepNode::Conditional {
            flag: sym(interner, *flag),
            sense: *sense,
            body: body
                .iter()
                .map(|c| normalize_depspec(c, interner))
                .collect(),
        },
        DepSpec::Leaf(atom) => DepNode::Leaf(normalize_atom(atom, interner)),
    }
}

/// Convert a single `moraine-atom` `Atom` into a `NormAtom`.
pub fn normalize_atom(atom: &Atom, interner: &Interner) -> NormAtom {
    let category = sym(interner, atom.category());
    let package = sym(interner, atom.package());
    let cp = format!("{category}/{package}");
    NormAtom {
        blocker: match atom.blocker() {
            Blocker::None => BlockerKind::None,
            Blocker::Weak => BlockerKind::Weak,
            Blocker::Strong => BlockerKind::Strong,
        },
        cp,
        version: atom.version().map(|(op, v)| (normalize_op(op), v.clone())),
        slot: atom.slot().map(|s| sym(interner, s)),
        subslot: atom.subslot().map(|s| sym(interner, s)),
        slot_op: atom.slot_op().map(|s| match s {
            SlotOp::Equal => SlotOpKind::Equal,
            SlotOp::Star => SlotOpKind::Star,
        }),
        use_deps: atom
            .use_deps()
            .iter()
            .map(|d| UseReq {
                flag: sym(interner, d.flag),
                kind: match d.kind {
                    UseDepKind::Enabled => UseReqKind::Enabled,
                    UseDepKind::Disabled => UseReqKind::Disabled,
                    UseDepKind::EnabledIfParent => UseReqKind::EnabledIfParent,
                    UseDepKind::DisabledIfParent => UseReqKind::DisabledIfParent,
                    UseDepKind::EqualToParent => UseReqKind::EqualToParent,
                    UseDepKind::OppositeToParent => UseReqKind::OppositeToParent,
                },
                default: d.default,
            })
            .collect(),
    }
}

fn normalize_op(op: Operator) -> Op {
    match op {
        Operator::Equal => Op::Equal,
        Operator::GreaterEqual => Op::GreaterEqual,
        Operator::LessEqual => Op::LessEqual,
        Operator::Greater => Op::Greater,
        Operator::Less => Op::Less,
        Operator::Tilde => Op::Tilde,
        Operator::EqualGlob => Op::EqualGlob,
    }
}

fn sym(interner: &Interner, s: moraine_common::Symbol) -> String {
    interner
        .resolve(s)
        .map(|a| a.to_string())
        .unwrap_or_default()
}
