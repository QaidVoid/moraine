//! Property tests for incremental stacking and USE_EXPAND flattening.

use std::collections::BTreeSet;

use moraine_config::makeconf::VarMap;
use moraine_config::stacking::stack_incremental;
use moraine_config::use_resolution::flatten_use_expand;
use proptest::prelude::*;

proptest! {
    #[test]
    fn stacking_dedups(tokens in prop::collection::vec(prop::sample::select(vec!["a", "b", "c"]), 0..12)) {
        let out = stack_incremental(tokens.iter().copied());
        let uniq: BTreeSet<&String> = out.iter().collect();
        prop_assert_eq!(uniq.len(), out.len());
    }

    #[test]
    fn negation_removes_token(token in prop::sample::select(vec!["a", "b", "c"])) {
        let neg = format!("-{token}");
        let out = stack_incremental(vec![token, neg.as_str()]);
        prop_assert!(!out.iter().any(|x| x == token));
    }

    #[test]
    fn wildcard_clears(prefix in prop::collection::vec(prop::sample::select(vec!["a", "b"]), 0..6)) {
        let mut toks: Vec<&str> = prefix.clone();
        toks.push("-*");
        toks.push("z");
        let out = stack_incremental(toks);
        prop_assert_eq!(out, vec!["z".to_owned()]);
    }

    #[test]
    fn use_expand_flatten_form(values in prop::collection::vec("[a-z0-9_]{1,6}", 1..4)) {
        let mut env = VarMap::new();
        env.set("USE_EXPAND", "PYTHON_TARGETS");
        env.set("PYTHON_TARGETS", values.join(" "));
        let (flags, _) = flatten_use_expand(&env);
        for v in &values {
            let want = format!("python_targets_{v}");
            prop_assert!(flags.contains(&want));
        }
    }
}
