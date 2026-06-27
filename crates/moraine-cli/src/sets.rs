//! Target and package-set expansion.
//!
//! Atom positionals and `@`-prefixed set tokens are expanded into a single flat
//! list of requested atom strings for the resolver. Set contents come from a
//! [`SetSource`], which the binary implements over `moraine-config` so the
//! standard sets are never re-read directly here. The trait also keeps the logic
//! testable with a fake source and no real Gentoo system.

use std::collections::BTreeSet;

use miette::Diagnostic;
use thiserror::Error;
use tracing::instrument;

/// A source of named package-set members.
///
/// `@world`, `@system`, and `@selected` are provided by `moraine-config`; user
/// sets resolve from the portage set search path. Members are atom strings,
/// which may themselves reference nested sets with an `@` prefix.
pub trait SetSource {
    /// The members of the named set, or `None` when the set is unknown.
    ///
    /// The name is given without the leading `@`.
    fn members(&self, name: &str) -> Option<Vec<String>>;
}

/// Errors from target and set expansion.
#[derive(Debug, Error, Diagnostic, PartialEq, Eq)]
pub enum ExpandError {
    /// An `@`-prefixed token named a set that does not exist.
    #[error("unknown package set `@{name}`")]
    #[diagnostic(
        code(moraine::sets::unknown),
        help(
            "known sets are @world, @system, @selected, @profile, @preserved-rebuild, @installed, @live-rebuild, and @module-rebuild, plus any user sets"
        )
    )]
    UnknownSet {
        /// The unknown set name, without the leading `@`.
        name: String,
    },

    /// A set's definition referenced itself, directly or through nesting.
    #[error("package set `@{name}` is defined in terms of itself")]
    #[diagnostic(code(moraine::sets::cycle))]
    CyclicSet {
        /// The set that closed the cycle.
        name: String,
    },
}

/// A fully expanded resolver request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    /// The flat list of requested atom strings, in stable order with
    /// duplicates removed.
    pub atoms: Vec<String>,
    /// Atoms excluded from the request, pinned to their installed version.
    pub excluded: Vec<String>,
    /// Whether the update modifier is active.
    pub update: bool,
    /// Whether the deep modifier is active.
    pub deep: bool,
    /// The optional `--deep` depth bound carried to the resolver; `None` is an
    /// unbounded `--deep`, and a depth of zero disables the deep consistency pass.
    pub deep_depth: Option<u32>,
    /// Whether the newuse modifier is active.
    pub newuse: bool,
    /// Whether the oneshot display note applies.
    pub oneshot: bool,
}

/// Modifiers applied to an expanded request.
#[derive(Debug, Clone, Copy, Default)]
pub struct Modifiers {
    /// Request the best visible version of each target.
    pub update: bool,
    /// Extend update behavior recursively across dependencies.
    pub deep: bool,
    /// The optional `--deep` depth bound; `None` means unbounded.
    pub deep_depth: Option<u32>,
    /// Reinstall packages whose effective USE set changed.
    pub newuse: bool,
    /// Do not add targets to the world set (display note only).
    pub oneshot: bool,
}

/// Expand raw target tokens and exclude atoms into a [`Request`].
///
/// Tokens beginning with `@` are looked up in `source` and expanded, including
/// nested set references. All other tokens are treated as atom strings. Excluded
/// atoms are removed from the resolved set and carried separately so the solver
/// is never asked to install or upgrade them.
#[instrument(skip(source, targets, excludes, modifiers), fields(targets = targets.len()))]
pub fn expand<S: SetSource>(
    source: &S,
    targets: &[String],
    excludes: &[String],
    modifiers: Modifiers,
) -> Result<Request, ExpandError> {
    let mut atoms: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for token in targets {
        for atom in expand_token(source, token)? {
            if seen.insert(atom.clone()) {
                atoms.push(atom);
            }
        }
    }

    let excluded: BTreeSet<String> = excludes.iter().cloned().collect();
    atoms.retain(|atom| !excluded.contains(atom));

    Ok(Request {
        atoms,
        excluded: excludes.to_vec(),
        update: modifiers.update,
        deep: modifiers.deep,
        deep_depth: modifiers.deep_depth,
        newuse: modifiers.newuse,
        oneshot: modifiers.oneshot,
    })
}

/// Expand a single token into its atom members, recursing through nested sets.
fn expand_token<S: SetSource>(source: &S, token: &str) -> Result<Vec<String>, ExpandError> {
    let mut out = Vec::new();
    let mut active: BTreeSet<String> = BTreeSet::new();
    expand_into(source, token, &mut out, &mut active)?;
    Ok(out)
}

fn expand_into<S: SetSource>(
    source: &S,
    token: &str,
    out: &mut Vec<String>,
    active: &mut BTreeSet<String>,
) -> Result<(), ExpandError> {
    let Some(name) = token.strip_prefix('@') else {
        out.push(token.to_owned());
        return Ok(());
    };

    if !active.insert(name.to_owned()) {
        return Err(ExpandError::CyclicSet {
            name: name.to_owned(),
        });
    }

    let members = source
        .members(name)
        .ok_or_else(|| ExpandError::UnknownSet {
            name: name.to_owned(),
        })?;
    for member in &members {
        expand_into(source, member, out, active)?;
    }

    active.remove(name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// A fake set source backed by a name-to-members map.
    struct FakeSource(BTreeMap<String, Vec<String>>);

    impl FakeSource {
        fn new(pairs: &[(&str, &[&str])]) -> Self {
            let map = pairs
                .iter()
                .map(|(name, members)| {
                    (
                        (*name).to_owned(),
                        members.iter().map(|m| (*m).to_owned()).collect(),
                    )
                })
                .collect();
            FakeSource(map)
        }
    }

    impl SetSource for FakeSource {
        fn members(&self, name: &str) -> Option<Vec<String>> {
            self.0.get(name).cloned()
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn mixed_atoms_and_sets_form_one_request() {
        let source = FakeSource::new(&[("world", &["cat/a", "cat/b"])]);
        let req = expand(
            &source,
            &strings(&["cat/x", "@world"]),
            &[],
            Modifiers::default(),
        )
        .unwrap();
        assert_eq!(req.atoms, strings(&["cat/x", "cat/a", "cat/b"]));
    }

    #[test]
    fn world_expands_to_selected_plus_system() {
        let source = FakeSource::new(&[
            ("selected", &["cat/a"]),
            ("system", &["cat/sys"]),
            ("world", &["@selected", "@system"]),
        ]);
        let req = expand(&source, &strings(&["@world"]), &[], Modifiers::default()).unwrap();
        assert_eq!(req.atoms, strings(&["cat/a", "cat/sys"]));
    }

    #[test]
    fn unknown_set_is_rejected() {
        let source = FakeSource::new(&[]);
        let err = expand(&source, &strings(&["@nope"]), &[], Modifiers::default()).unwrap_err();
        assert_eq!(
            err,
            ExpandError::UnknownSet {
                name: "nope".to_owned()
            }
        );
    }

    #[test]
    fn excluded_atom_is_removed() {
        let source = FakeSource::new(&[("world", &["cat/a", "cat/keep"])]);
        let req = expand(
            &source,
            &strings(&["@world"]),
            &strings(&["cat/a"]),
            Modifiers::default(),
        )
        .unwrap();
        assert_eq!(req.atoms, strings(&["cat/keep"]));
        assert_eq!(req.excluded, strings(&["cat/a"]));
    }

    #[test]
    fn duplicate_members_are_deduplicated() {
        let source = FakeSource::new(&[("a", &["cat/dup"]), ("b", &["cat/dup"])]);
        let req = expand(&source, &strings(&["@a", "@b"]), &[], Modifiers::default()).unwrap();
        assert_eq!(req.atoms, strings(&["cat/dup"]));
    }

    #[test]
    fn cyclic_set_is_rejected() {
        let source = FakeSource::new(&[("a", &["@b"]), ("b", &["@a"])]);
        let err = expand(&source, &strings(&["@a"]), &[], Modifiers::default()).unwrap_err();
        assert_eq!(
            err,
            ExpandError::CyclicSet {
                name: "a".to_owned()
            }
        );
    }

    #[test]
    fn modifiers_are_carried() {
        let source = FakeSource::new(&[("world", &["cat/a"])]);
        let req = expand(
            &source,
            &strings(&["@world"]),
            &[],
            Modifiers {
                update: true,
                deep: true,
                deep_depth: Some(2),
                newuse: true,
                oneshot: true,
            },
        )
        .unwrap();
        assert!(req.update && req.deep && req.newuse && req.oneshot);
        assert_eq!(req.deep_depth, Some(2));
    }
}
