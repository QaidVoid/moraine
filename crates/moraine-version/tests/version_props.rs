//! Property tests for version ordering laws and render round-trips.

use std::cmp::Ordering;

use moraine_version::Version;
use proptest::prelude::*;

fn build(
    first: u32,
    dotted: Vec<u32>,
    letter: Option<u8>,
    suffixes: Vec<(usize, u32)>,
    rev: u32,
) -> String {
    let mut s = first.to_string();
    for d in &dotted {
        s.push('.');
        s.push_str(&d.to_string());
    }
    if let Some(l) = letter {
        s.push(l as char);
    }
    let names = ["alpha", "beta", "pre", "rc", "p"];
    for (kind, number) in &suffixes {
        s.push('_');
        s.push_str(names[*kind]);
        if *number > 0 {
            s.push_str(&number.to_string());
        }
    }
    if rev > 0 {
        s.push_str(&format!("-r{rev}"));
    }
    s
}

prop_compose! {
    fn arb_version()(
        first in 0u32..30,
        dotted in prop::collection::vec(0u32..30, 0..3),
        letter in prop::option::of(b'a'..=b'c'),
        suffixes in prop::collection::vec((0usize..5, 0u32..4), 0..2),
        rev in 0u32..4,
    ) -> Version {
        Version::parse(&build(first, dotted, letter, suffixes, rev)).unwrap()
    }
}

proptest! {
    #[test]
    fn render_roundtrips(a in arb_version()) {
        let b = Version::parse(&a.to_string()).unwrap();
        prop_assert_eq!(a.cmp(&b), Ordering::Equal);
    }

    #[test]
    fn order_is_antisymmetric_and_reflexive(a in arb_version(), b in arb_version()) {
        prop_assert_eq!(a.cmp(&b), b.cmp(&a).reverse());
        prop_assert_eq!(a.cmp(&a), Ordering::Equal);
    }

    #[test]
    fn order_is_transitive(a in arb_version(), b in arb_version(), c in arb_version()) {
        if a <= b && b <= c {
            prop_assert!(a <= c);
        }
    }
}
