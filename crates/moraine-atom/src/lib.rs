//! Package atoms and the dependency-string AST.
//!
//! [`Atom`] parses a package atom under a given EAPI feature set, interning its
//! tokens through [`moraine_common`]. [`DepSpec`] parses a DEPEND-style string
//! into a typed tree. Both are the typed equivalents of stock Portage's atom and
//! `use_reduce` machinery and feed the resolver in later phases.

pub mod atom;
pub mod depspec;
mod error;

pub use atom::{Atom, Blocker, Operator, PackageRef, SlotOp, UseDep, UseDepKind, UseRequirement};
pub use depspec::DepSpec;
pub use error::{AtomError, DepError};

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use moraine_common::Interner;
    use moraine_eapi::{PERMISSIVE, features_for_level};
    use moraine_version::Version;

    use super::*;

    fn eapi8() -> moraine_eapi::EapiFeatures {
        features_for_level(8)
    }

    #[test]
    fn plain_cp_parses() {
        let i = Interner::new();
        let a = Atom::parse("dev-libs/openssl", eapi8(), &i).unwrap();
        assert_eq!(a.category(), i.intern("dev-libs"));
        assert_eq!(a.package(), i.intern("openssl"));
        assert!(a.version().is_none());
        assert_eq!(a.blocker(), Blocker::None);
    }

    #[test]
    fn versioned_atom_parses() {
        let i = Interner::new();
        let a = Atom::parse(">=dev-libs/openssl-3.0", eapi8(), &i).unwrap();
        let (op, ver) = a.version().unwrap();
        assert_eq!(op, Operator::GreaterEqual);
        assert_eq!(ver, &Version::parse("3.0").unwrap());
    }

    #[test]
    fn invalid_atom_is_rejected() {
        let i = Interner::new();
        assert!(Atom::parse("not an atom", eapi8(), &i).is_err());
        assert!(Atom::parse("missingcategory", eapi8(), &i).is_err());
        assert!(Atom::parse(">=dev-libs/openssl", eapi8(), &i).is_err());
    }

    #[test]
    fn tilde_matches_any_revision() {
        let i = Interner::new();
        let a = Atom::parse("~dev-libs/openssl-3.0", eapi8(), &i).unwrap();
        let ver = Version::parse("3.0-r2").unwrap();
        let cand = PackageRef {
            category: i.intern("dev-libs"),
            package: i.intern("openssl"),
            version: &ver,
            slot: None,
            subslot: None,
            repo: None,
        };
        assert!(a.matches(&cand));
    }

    #[test]
    fn prefix_glob_matches_by_prefix() {
        let i = Interner::new();
        let a = Atom::parse("=dev-libs/openssl-3.0*", eapi8(), &i).unwrap();
        let yes = Version::parse("3.0.1").unwrap();
        let no = Version::parse("3.1").unwrap();
        let base = PackageRef {
            category: i.intern("dev-libs"),
            package: i.intern("openssl"),
            version: &yes,
            slot: None,
            subslot: None,
            repo: None,
        };
        assert!(a.matches(&base));
        assert!(!a.matches(&PackageRef {
            version: &no,
            ..base
        }));
    }

    #[test]
    fn prefix_glob_respects_component_boundary() {
        let i = Interner::new();
        let a = Atom::parse("=dev-libs/openssl-3.0*", eapi8(), &i).unwrap();
        // `3.05` shares the textual prefix `3.0` but its next character is a
        // digit, so it is a different version component and must not match.
        let no = Version::parse("3.05").unwrap();
        let base = PackageRef {
            category: i.intern("dev-libs"),
            package: i.intern("openssl"),
            version: &no,
            slot: None,
            subslot: None,
            repo: None,
        };
        assert!(!a.matches(&base));
    }

    #[test]
    fn tilde_with_revision_is_rejected() {
        let i = Interner::new();
        assert!(Atom::parse("~dev-libs/openssl-3.0-r1", eapi8(), &i).is_err());
        assert!(Atom::parse("~dev-libs/openssl-3.0", eapi8(), &i).is_ok());
    }

    #[test]
    fn invalid_repo_and_flag_charsets_are_rejected() {
        let i = Interner::new();
        // Repository names may not contain `.` (repo specifiers need PERMISSIVE).
        assert!(Atom::parse("dev-libs/openssl::bad.repo", PERMISSIVE, &i).is_err());
        assert!(Atom::parse("dev-libs/openssl::good_repo", PERMISSIVE, &i).is_ok());
        // USE-flag names may not contain `.`.
        assert!(Atom::parse("dev-libs/openssl[bad.flag]", eapi8(), &i).is_err());
        assert!(Atom::parse("dev-libs/openssl[good_flag]", eapi8(), &i).is_ok());
    }

    #[test]
    fn exactly_one_and_at_most_one_rejected_in_depspec() {
        let i = Interner::new();
        assert!(DepSpec::parse("^^ ( a/b c/d )", eapi8(), &i).is_err());
        assert!(DepSpec::parse("?? ( a/b c/d )", eapi8(), &i).is_err());
        assert!(DepSpec::parse("|| ( a/b c/d )", eapi8(), &i).is_ok());
    }

    #[test]
    fn slot_parses_and_slot_operator_gating() {
        let i = Interner::new();
        let a = Atom::parse("dev-libs/openssl:0", eapi8(), &i).unwrap();
        assert_eq!(a.slot(), Some(i.intern("0")));

        // Slot operator rejected under EAPI 4 (no slot_operator).
        let eapi4 = features_for_level(4);
        assert!(Atom::parse("dev-libs/openssl:=", eapi4, &i).is_err());
        // Accepted under EAPI 5+.
        let ok = Atom::parse("dev-libs/openssl:=", eapi8(), &i).unwrap();
        assert_eq!(ok.slot_op(), Some(SlotOp::Equal));
    }

    #[test]
    fn use_deps_parse_and_default_gating() {
        let i = Interner::new();
        let a = Atom::parse("dev-libs/openssl[ssl,-bindist]", eapi8(), &i).unwrap();
        let deps = a.use_deps();
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].flag, i.intern("ssl"));
        assert_eq!(deps[0].kind, UseDepKind::Enabled);
        assert_eq!(deps[1].flag, i.intern("bindist"));
        assert_eq!(deps[1].kind, UseDepKind::Disabled);

        // (+) default requires EAPI 4.
        let eapi3 = features_for_level(3);
        assert!(Atom::parse("dev-libs/openssl[ssl(+)]", eapi3, &i).is_err());
        assert!(Atom::parse("dev-libs/openssl[ssl(+)]", eapi8(), &i).is_ok());
    }

    #[test]
    fn conditional_use_evaluates_against_parent_use() {
        let i = Interner::new();
        let a = Atom::parse("dev-libs/foo[bar?]", eapi8(), &i).unwrap();
        let bar = i.intern("bar");

        let mut enabled = HashSet::new();
        enabled.insert(bar);
        let req = a.evaluate_use(&enabled);
        assert_eq!(req.len(), 1);
        assert_eq!(req[0].flag, bar);
        assert!(req[0].enabled);

        let disabled = HashSet::new();
        assert!(a.evaluate_use(&disabled).is_empty());
    }

    #[test]
    fn blockers_and_repository() {
        let i = Interner::new();
        assert_eq!(
            Atom::parse("!dev-libs/foo", eapi8(), &i).unwrap().blocker(),
            Blocker::Weak
        );
        assert_eq!(
            Atom::parse("!!dev-libs/foo", eapi8(), &i)
                .unwrap()
                .blocker(),
            Blocker::Strong
        );
        // Repository specifier requires repo_deps (permissive fallback).
        let a = Atom::parse("dev-libs/foo::gentoo", PERMISSIVE, &i).unwrap();
        assert_eq!(a.repo(), Some(i.intern("gentoo")));
    }

    #[test]
    fn matching_rejects_on_cp_and_honors_constraints() {
        let i = Interner::new();
        let a = Atom::parse(">=dev-libs/openssl-3.0:0::gentoo", PERMISSIVE, &i).unwrap();
        let ver = Version::parse("3.1").unwrap();

        let other = PackageRef {
            category: i.intern("dev-libs"),
            package: i.intern("libxml2"),
            version: &ver,
            slot: Some(i.intern("0")),
            subslot: None,
            repo: Some(i.intern("gentoo")),
        };
        assert!(!a.matches(&other));

        let good = PackageRef {
            category: i.intern("dev-libs"),
            package: i.intern("openssl"),
            version: &ver,
            slot: Some(i.intern("0")),
            subslot: None,
            repo: Some(i.intern("gentoo")),
        };
        assert!(a.matches(&good));

        let wrong_slot = PackageRef {
            slot: Some(i.intern("1")),
            ..good
        };
        assert!(!a.matches(&wrong_slot));
    }

    #[test]
    fn render_roundtrips() {
        let i = Interner::new();
        for s in [
            "dev-libs/openssl",
            ">=dev-libs/openssl-3.0",
            "~dev-libs/openssl-3.0",
            "=dev-libs/openssl-3.0*",
            "dev-libs/openssl:0/1=",
            "dev-libs/foo[ssl,-bindist,python?]",
            "!!sys-apps/baz",
        ] {
            let a = Atom::parse(s, eapi8(), &i).unwrap();
            let b = Atom::parse(&a.render(&i), eapi8(), &i).unwrap();
            assert_eq!(a, b, "round-trip failed for {s}");
        }
    }

    #[test]
    fn depspec_flat_and_nested() {
        let i = Interner::new();
        let flat = DepSpec::parse("dev-libs/a dev-libs/b", eapi8(), &i).unwrap();
        assert_eq!(flat.atoms().len(), 2);

        let any = DepSpec::parse("|| ( dev-libs/a dev-libs/b )", eapi8(), &i).unwrap();
        match &any {
            DepSpec::AllOf(items) => {
                assert!(matches!(items.as_slice(), [DepSpec::AnyOf(_)]));
            }
            _ => panic!("expected all-of top level"),
        }

        let nested = DepSpec::parse(
            "dev-libs/a || ( dev-libs/b ( dev-libs/c dev-libs/d ) )",
            eapi8(),
            &i,
        )
        .unwrap();
        assert_eq!(nested.atoms().len(), 4);
    }

    #[test]
    fn depspec_conditional_eval() {
        let i = Interner::new();
        let ssl = i.intern("ssl");
        let spec = DepSpec::parse("ssl? ( dev-libs/openssl )", eapi8(), &i).unwrap();

        let mut enabled = HashSet::new();
        enabled.insert(ssl);
        assert_eq!(spec.evaluate(&enabled).atoms().len(), 1);

        let disabled = HashSet::new();
        assert_eq!(spec.evaluate(&disabled).atoms().len(), 0);
        // Source AST unchanged.
        assert_eq!(spec.atoms().len(), 1);
    }

    #[test]
    fn depspec_rejects_malformed() {
        let i = Interner::new();
        assert!(DepSpec::parse("|| ( dev-libs/a", eapi8(), &i).is_err());
        assert!(DepSpec::parse("|| dev-libs/a", eapi8(), &i).is_err());
        assert!(DepSpec::parse("dev-libs/a )", eapi8(), &i).is_err());
    }
}
