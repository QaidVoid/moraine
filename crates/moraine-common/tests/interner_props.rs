//! Property tests for the string interner invariants.

use moraine_common::Interner;
use proptest::prelude::*;

proptest! {
    #[test]
    fn intern_then_resolve_roundtrips(s in ".*") {
        let interner = Interner::new();
        let sym = interner.intern(&s);
        let resolved = interner.resolve(sym);
        prop_assert_eq!(resolved.as_deref(), Some(s.as_str()));
    }

    #[test]
    fn interning_is_idempotent(s in ".*") {
        let interner = Interner::new();
        let first = interner.intern(&s);
        let second = interner.intern(&s);
        prop_assert_eq!(first, second);
    }

    #[test]
    fn distinct_strings_never_collide(a in "[a-z]{1,16}", b in "[a-z]{1,16}") {
        prop_assume!(a != b);
        let interner = Interner::new();
        prop_assert_ne!(interner.intern(&a), interner.intern(&b));
    }
}
