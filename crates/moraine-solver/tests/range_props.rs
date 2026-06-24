//! Property tests for the range-algebra laws.

use std::ops::Bound;

use moraine_solver::Range;
use proptest::prelude::*;

fn arb_range() -> impl Strategy<Value = Range<i32>> {
    prop::collection::vec((0i32..16, 0i32..16), 0..4).prop_map(|pairs| {
        let mut r = Range::empty();
        for (a, b) in pairs {
            let lo = a.min(b);
            let hi = a.max(b) + 1;
            r = r.union(&Range::interval(Bound::Included(lo), Bound::Excluded(hi)));
        }
        r
    })
}

proptest! {
    #[test]
    fn idempotence(a in arb_range()) {
        prop_assert_eq!(a.union(&a), a.clone());
        prop_assert_eq!(a.intersection(&a), a);
    }

    #[test]
    fn commutativity(a in arb_range(), b in arb_range()) {
        prop_assert_eq!(a.union(&b), b.union(&a));
        prop_assert_eq!(a.intersection(&b), b.intersection(&a));
    }

    #[test]
    fn associativity(a in arb_range(), b in arb_range(), c in arb_range()) {
        prop_assert_eq!(a.union(&b).union(&c), a.union(&b.union(&c)));
        prop_assert_eq!(a.intersection(&b).intersection(&c), a.intersection(&b.intersection(&c)));
    }

    #[test]
    fn de_morgan(a in arb_range(), b in arb_range()) {
        prop_assert_eq!(a.intersection(&b).complement(), a.complement().union(&b.complement()));
    }

    #[test]
    fn complement_involution(a in arb_range()) {
        prop_assert_eq!(a.complement().complement(), a);
    }

    #[test]
    fn identities(a in arb_range()) {
        prop_assert_eq!(a.union(&Range::empty()), a.clone());
        prop_assert_eq!(a.intersection(&Range::full()), a.clone());
        prop_assert_eq!(a.union(&Range::full()), Range::full());
        prop_assert_eq!(a.intersection(&Range::empty()), Range::empty());
    }
}
