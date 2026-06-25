//! License acceptance: `license_groups` expansion, `ACCEPT_LICENSE`, and
//! `package.license`, mirroring Portage's `LicenseManager`.
//!
//! A package's `LICENSE` is a USE-conditional dependency string whose leaves are
//! license tokens. The resolver reduces it against the package's USE into a
//! [`LicenseReq`] tree (conditionals already resolved) and asks
//! [`LicenseManager::missing_licenses`] which leaves are not accepted. A package
//! with a non-empty missing set is masked.

use std::collections::{BTreeMap, BTreeSet};

use moraine_atom::{Atom, PackageRef};

/// A USE-reduced `LICENSE` requirement tree: all conditional groups have been
/// resolved against the package's USE, leaving only all-of and any-of structure
/// over license-token leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseReq {
    /// A single license token.
    Token(String),
    /// Every child must be accepted.
    AllOf(Vec<LicenseReq>),
    /// At least one child must be fully accepted (`|| ( ... )`).
    AnyOf(Vec<LicenseReq>),
}

/// The effective acceptance state after folding `ACCEPT_LICENSE` (and any
/// per-package `package.license` overrides). `*` accepts everything except the
/// tokens later removed; otherwise only the explicitly added tokens are
/// accepted.
#[derive(Debug, Clone, Default)]
struct AcceptState {
    accept_all: bool,
    set: BTreeSet<String>,
}

impl AcceptState {
    /// Apply one already-expanded token (`*`, `-*`, `-token`, or `token`).
    fn apply(&mut self, token: &str) {
        match token {
            "*" => {
                self.accept_all = true;
                self.set.clear();
            }
            "-*" => {
                self.accept_all = false;
                self.set.clear();
            }
            _ => {
                if let Some(name) = token.strip_prefix('-') {
                    if self.accept_all {
                        self.set.insert(name.to_owned());
                    } else {
                        self.set.remove(name);
                    }
                } else if self.accept_all {
                    self.set.remove(token);
                } else {
                    self.set.insert(token.to_owned());
                }
            }
        }
    }

    /// Whether a concrete license token is accepted.
    fn accepts(&self, license: &str) -> bool {
        if self.accept_all {
            !self.set.contains(license)
        } else {
            self.set.contains(license)
        }
    }
}

/// A `package.license` entry: an atom and its already-expanded license tokens.
#[derive(Debug, Clone)]
pub struct PkgLicenseEntry {
    /// The atom the entry applies to.
    pub atom: Atom,
    /// The license tokens (`token`, `-token`, `*`, `-*`), `@group` pre-expanded.
    pub tokens: Vec<String>,
}

/// Resolves which of a package's licenses are not accepted, from
/// `license_groups`, `ACCEPT_LICENSE`, and `package.license`.
#[derive(Debug, Clone, Default)]
pub struct LicenseManager {
    groups: BTreeMap<String, Vec<String>>,
    global: AcceptState,
    pkg_license: Vec<PkgLicenseEntry>,
}

impl LicenseManager {
    /// Build a manager from the `license_groups` map, the raw (unexpanded)
    /// `ACCEPT_LICENSE` tokens, and the `package.license` entries (whose tokens
    /// are expanded here).
    pub fn new(
        groups: BTreeMap<String, Vec<String>>,
        accept_license: &[String],
        pkg_license: Vec<(Atom, Vec<String>)>,
    ) -> Self {
        let mut mgr = LicenseManager {
            groups,
            global: AcceptState::default(),
            pkg_license: Vec::new(),
        };
        // Fold the expanded global ACCEPT_LICENSE.
        let expanded = mgr.expand_license_tokens(accept_license);
        for token in &expanded {
            mgr.global.apply(token);
        }
        // Expand and store the per-package entries, most specific last.
        mgr.pkg_license = pkg_license
            .into_iter()
            .map(|(atom, tokens)| PkgLicenseEntry {
                tokens: mgr.expand_license_tokens(&tokens),
                atom,
            })
            .collect();
        mgr.pkg_license.sort_by_key(|e| specificity(&e.atom));
        mgr
    }

    /// Expand `@group` and `-@group` tokens transitively against the
    /// `license_groups`, propagating negation. An undefined group is kept
    /// verbatim; a circular reference stops expanding that branch.
    pub fn expand_license_tokens(&self, tokens: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        for token in tokens {
            self.expand_one(token, &mut out, &mut BTreeSet::new());
        }
        out
    }

    fn expand_one(&self, token: &str, out: &mut Vec<String>, visiting: &mut BTreeSet<String>) {
        let (negate, body) = match token.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, token),
        };
        let Some(group) = body.strip_prefix('@') else {
            out.push(token.to_owned());
            return;
        };
        if !self.groups.contains_key(group) {
            // Undefined group: keep the token verbatim (Portage warns once).
            out.push(token.to_owned());
            return;
        }
        if !visiting.insert(group.to_owned()) {
            // Circular reference: stop expanding this branch.
            return;
        }
        for member in &self.groups[group] {
            // Within a negated group every member is negated; a member that is
            // itself negated flips back.
            let resolved = if negate {
                match member.strip_prefix('-') {
                    Some(rest) => rest.to_owned(),
                    None => format!("-{member}"),
                }
            } else {
                member.clone()
            };
            self.expand_one(&resolved, out, visiting);
        }
        visiting.remove(group);
    }

    /// The licenses of `reduced` that are not accepted for `pkg`. An empty result
    /// means the package's license is acceptable.
    pub fn missing_licenses(&self, reduced: &LicenseReq, pkg: &PackageRef<'_>) -> BTreeSet<String> {
        let state = self.accept_state_for(pkg);
        missing(reduced, &state)
    }

    fn accept_state_for(&self, pkg: &PackageRef<'_>) -> AcceptState {
        let mut state = self.global.clone();
        for entry in &self.pkg_license {
            if entry.atom.matches(pkg) {
                for token in &entry.tokens {
                    state.apply(token);
                }
            }
        }
        state
    }
}

fn missing(req: &LicenseReq, state: &AcceptState) -> BTreeSet<String> {
    match req {
        LicenseReq::Token(t) => {
            if state.accepts(t) {
                BTreeSet::new()
            } else {
                BTreeSet::from([t.clone()])
            }
        }
        LicenseReq::AllOf(children) => children.iter().flat_map(|c| missing(c, state)).collect(),
        LicenseReq::AnyOf(children) => {
            let per: Vec<BTreeSet<String>> = children.iter().map(|c| missing(c, state)).collect();
            if per.iter().any(|m| m.is_empty()) {
                BTreeSet::new()
            } else {
                per.into_iter().flatten().collect()
            }
        }
    }
}

/// A specificity score for ordering `package.license` entries (more specific
/// applied last).
fn specificity(atom: &Atom) -> u32 {
    let mut score = 0;
    if atom.version().is_some() {
        score += 4;
    }
    if atom.slot().is_some() {
        score += 2;
    }
    if atom.repo().is_some() {
        score += 1;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use moraine_common::Interner;
    use moraine_version::Version;

    fn groups() -> BTreeMap<String, Vec<String>> {
        let mut g = BTreeMap::new();
        g.insert(
            "FREE".to_owned(),
            vec!["GPL-2".to_owned(), "BSD".to_owned()],
        );
        // BINARY-REDISTRIBUTABLE nests @FREE plus a binary license.
        g.insert(
            "BINARY-REDISTRIBUTABLE".to_owned(),
            vec!["@FREE".to_owned(), "freedist".to_owned()],
        );
        g.insert("EULA".to_owned(), vec!["skype-eula".to_owned()]);
        g
    }

    fn pkg<'a>(i: &Interner, v: &'a Version) -> PackageRef<'a> {
        PackageRef {
            category: i.intern("dev-libs"),
            package: i.intern("foo"),
            version: v,
            slot: None,
            subslot: None,
            repo: None,
        }
    }

    #[test]
    fn group_expands_transitively() {
        let mgr = LicenseManager::new(groups(), &[], Vec::new());
        let out = mgr.expand_license_tokens(&["@BINARY-REDISTRIBUTABLE".to_owned()]);
        assert!(out.contains(&"GPL-2".to_owned()));
        assert!(out.contains(&"BSD".to_owned()));
        assert!(out.contains(&"freedist".to_owned()));
    }

    #[test]
    fn negated_group_negates_members() {
        let mgr = LicenseManager::new(groups(), &[], Vec::new());
        let out = mgr.expand_license_tokens(&["-@FREE".to_owned()]);
        assert_eq!(out, vec!["-GPL-2".to_owned(), "-BSD".to_owned()]);
    }

    #[test]
    fn undefined_group_kept_verbatim() {
        let mgr = LicenseManager::new(groups(), &[], Vec::new());
        let out = mgr.expand_license_tokens(&["@NOPE".to_owned()]);
        assert_eq!(out, vec!["@NOPE".to_owned()]);
    }

    #[test]
    fn circular_group_terminates() {
        let mut g = BTreeMap::new();
        g.insert("A".to_owned(), vec!["@B".to_owned(), "x".to_owned()]);
        g.insert("B".to_owned(), vec!["@A".to_owned(), "y".to_owned()]);
        let mgr = LicenseManager::new(g, &[], Vec::new());
        let out = mgr.expand_license_tokens(&["@A".to_owned()]);
        assert!(out.contains(&"x".to_owned()) && out.contains(&"y".to_owned()));
    }

    #[test]
    fn accept_all_with_eula_removed() {
        // The default `* -@EULA` gate accepts everything but EULA licenses.
        let mgr = LicenseManager::new(groups(), &["*".to_owned(), "-@EULA".to_owned()], Vec::new());
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        assert!(
            mgr.missing_licenses(&LicenseReq::Token("GPL-2".to_owned()), &pkg(&i, &v))
                .is_empty()
        );
        assert_eq!(
            mgr.missing_licenses(&LicenseReq::Token("skype-eula".to_owned()), &pkg(&i, &v)),
            BTreeSet::from(["skype-eula".to_owned()])
        );
    }

    #[test]
    fn any_of_accepts_when_one_branch_accepted() {
        let mgr = LicenseManager::new(BTreeMap::new(), &["GPL-2".to_owned()], Vec::new());
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let req = LicenseReq::AnyOf(vec![
            LicenseReq::Token("GPL-2".to_owned()),
            LicenseReq::Token("commercial".to_owned()),
        ]);
        assert!(mgr.missing_licenses(&req, &pkg(&i, &v)).is_empty());
        // Neither branch accepted: the group is missing.
        let mgr2 = LicenseManager::new(BTreeMap::new(), &["BSD".to_owned()], Vec::new());
        assert!(!mgr2.missing_licenses(&req, &pkg(&i, &v)).is_empty());
    }

    #[test]
    fn package_license_adds_acceptance() {
        let i = Interner::new();
        let atom = Atom::parse("dev-libs/foo", moraine_eapi::PERMISSIVE, &i).unwrap();
        let mgr = LicenseManager::new(
            groups(),
            &["*".to_owned(), "-@EULA".to_owned()],
            vec![(atom, vec!["skype-eula".to_owned()])],
        );
        let v = Version::parse("1.0").unwrap();
        // package.license re-accepts the EULA token for this package.
        assert!(
            mgr.missing_licenses(&LicenseReq::Token("skype-eula".to_owned()), &pkg(&i, &v))
                .is_empty()
        );
    }
}
