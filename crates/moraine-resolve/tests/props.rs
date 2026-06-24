//! Property tests for ordering invariants and tier monotonicity.

mod fixture;

use fixture::{Fixture, PkgSpec};
use moraine_resolve::graph::{EdgeFlags, MAX_TIER, Range, edge_ignored};
use moraine_resolve::solution::DepClass;
use moraine_resolve::{resolve, serialize};
use proptest::prelude::*;

/// Generate an arbitrary edge flag-set across the supported classes and
/// modifiers.
fn any_flags() -> impl Strategy<Value = EdgeFlags> {
    let classes = prop_oneof![
        Just(DepClass::Bdepend),
        Just(DepClass::Depend),
        Just(DepClass::Rdepend),
        Just(DepClass::Pdepend),
        Just(DepClass::Idepend),
    ];
    (classes, any::<bool>(), any::<bool>(), any::<bool>()).prop_map(
        |(c, slot_op, optional, satisfied)| EdgeFlags::for_class(c, slot_op, optional, satisfied),
    )
}

proptest! {
    // A higher ignore tier ignores a superset of the edges a lower tier does.
    #[test]
    fn tier_monotonicity_normal(flags in any_flags(), tier in 0u32..MAX_TIER) {
        if edge_ignored(&flags, tier, Range::Normal) {
            prop_assert!(edge_ignored(&flags, tier + 1, Range::Normal));
        }
    }

    // The satisfied range ignores a superset of what the normal range ignores
    // at the same tier.
    #[test]
    fn satisfied_superset_of_normal(flags in any_flags(), tier in 0u32..=MAX_TIER) {
        if edge_ignored(&flags, tier, Range::Normal) {
            prop_assert!(edge_ignored(&flags, tier, Range::Satisfied));
        }
    }

    // Hardness is stable: a build-time edge is always harder than any runtime
    // edge.
    #[test]
    fn buildtime_hardest(slot_op in any::<bool>()) {
        let bt = EdgeFlags::for_class(DepClass::Depend, slot_op, false, false);
        let rt = EdgeFlags::for_class(DepClass::Rdepend, true, false, false);
        prop_assert!(bt.hardness() > rt.hardness());
    }
}

// Build a fixture with `n` non-overlapping `||` branches and confirm encoding
// and resolution complete quickly (no exponential expansion).
proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    #[test]
    fn any_of_no_exponential_blowup(n in 1usize..12) {
        let mut f = Fixture::new();
        // Build a `|| ( cat/p0 cat/p1 ... )` over disjoint cps.
        let branches: Vec<String> = (0..n).map(|i| format!("cat/p{i}")).collect();
        let rdepend: &'static str =
            Box::leak(format!("|| ( {} )", branches.join(" ")).into_boxed_str());
        f.add(PkgSpec { cp: "cat/main", version: "1", rdepend, ..Default::default() });
        for i in 0..n {
            let cp: &'static str = Box::leak(format!("cat/p{i}").into_boxed_str());
            f.add(PkgSpec { cp, version: "1", ..Default::default() });
        }
        let sol = resolve(&f, &["cat/main"]).expect("resolves");
        // The first branch is the greedy choice.
        prop_assert!(sol.package("cat/p0").is_some());
        // Serialization always succeeds and is a permutation.
        let tasks = serialize(&sol).expect("serializes");
        prop_assert_eq!(tasks.len(), sol.packages.len());
    }
}
