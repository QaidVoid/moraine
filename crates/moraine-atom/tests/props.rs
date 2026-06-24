//! Property tests for atom round-trips and dependency-spec parsing/evaluation.

use std::collections::HashSet;

use moraine_atom::{Atom, DepSpec};
use moraine_common::Interner;
use moraine_eapi::features_for_level;
use proptest::prelude::*;

fn ver_str(first: u32, rest: Vec<u32>) -> String {
    let mut s = first.to_string();
    for r in rest {
        s.push('.');
        s.push_str(&r.to_string());
    }
    s
}

prop_compose! {
    fn arb_atom_string()(
        cat in prop::sample::select(vec!["dev-libs", "sys-apps", "app-misc"]),
        pkg in prop::sample::select(vec!["foo", "bar", "openssl", "libxml2"]),
        op_idx in 0usize..5,
        first in 0u32..20,
        rest in prop::collection::vec(0u32..20, 0..2),
        slot in prop::option::of(prop::sample::select(vec!["0", "1", "2"])),
    ) -> String {
        let ops = ["", "=", ">=", "<=", "~"];
        let op = ops[op_idx];
        let mut s = String::new();
        s.push_str(op);
        s.push_str(cat);
        s.push('/');
        s.push_str(pkg);
        if !op.is_empty() {
            s.push('-');
            s.push_str(&ver_str(first, rest));
        }
        if let Some(sl) = slot {
            s.push(':');
            s.push_str(sl);
        }
        s
    }
}

proptest! {
    #[test]
    fn atom_renders_roundtrip(s in arb_atom_string()) {
        let i = Interner::new();
        let f = features_for_level(8);
        let a = Atom::parse(&s, f, &i).unwrap();
        let b = Atom::parse(&a.render(&i), f, &i).unwrap();
        prop_assert_eq!(a, b);
    }
}

fn arb_dep() -> impl Strategy<Value = String> {
    let leaf = prop::sample::select(vec!["dev-libs/a", "dev-libs/b", "sys-apps/c"])
        .prop_map(|s| s.to_string());
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..3).prop_map(|v| v.join(" ")),
            prop::collection::vec(inner.clone(), 1..3)
                .prop_map(|v| format!("|| ( {} )", v.join(" "))),
            prop::collection::vec(inner, 1..3).prop_map(|v| format!("flag? ( {} )", v.join(" "))),
        ]
    })
}

proptest! {
    #[test]
    fn dep_parses_and_evaluation_does_not_mutate(s in arb_dep()) {
        let i = Interner::new();
        let f = features_for_level(8);
        let spec = DepSpec::parse(&s, f, &i).unwrap();
        let before = spec.atoms().len();

        let mut use_set = HashSet::new();
        use_set.insert(i.intern("flag"));
        let _ = spec.evaluate(&use_set);
        let _ = spec.evaluate(&HashSet::new());

        // The source AST is unchanged by evaluation.
        prop_assert_eq!(spec.atoms().len(), before);
    }
}
