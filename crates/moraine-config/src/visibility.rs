//! Package visibility: masking, keyword acceptance, and `package.provided`.

use std::collections::BTreeSet;

use moraine_atom::{Atom, PackageRef};

/// The reason a package is or is not visible by keywords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordResult {
    /// The package is keyword-accepted.
    Accepted,
    /// The package needs a (testing) keyword to be accepted.
    NeedsKeyword,
    /// The package has empty or broken keywords and needs `**`.
    NeedsDoubleStar,
}

/// Decide keyword acceptance for a package.
///
/// `keywords` is the package's `KEYWORDS`, `accepted` is the effective accepted
/// keyword set (`ACCEPT_KEYWORDS` plus any per-package additions), and `arch` is
/// the stable architecture keyword.
pub fn accept_keywords(
    keywords: &[String],
    accepted: &BTreeSet<String>,
    arch: &str,
) -> KeywordResult {
    if accepted.contains("**") {
        return KeywordResult::Accepted;
    }
    let mut saw_real = false;
    for kw in keywords {
        if kw == "-*" {
            continue;
        }
        if let Some(base) = kw.strip_prefix('-') {
            if base == arch || base == "*" {
                return KeywordResult::NeedsDoubleStar;
            }
            continue;
        }
        saw_real = true;
        if accepted.contains(kw) {
            return KeywordResult::Accepted;
        }
        if kw.starts_with('~') {
            if accepted.contains("~*") {
                return KeywordResult::Accepted;
            }
        } else {
            if accepted.contains("*") {
                return KeywordResult::Accepted;
            }
            if accepted.contains(&format!("~{kw}")) {
                return KeywordResult::Accepted;
            }
        }
    }
    if !saw_real {
        return KeywordResult::NeedsDoubleStar;
    }
    KeywordResult::NeedsKeyword
}

/// Stacks `package.mask` / `package.unmask` and answers whether a package is
/// masked.
#[derive(Debug, Clone, Default)]
pub struct MaskManager {
    masks: Vec<Atom>,
    unmasks: Vec<Atom>,
}

impl MaskManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a masking atom (a `package.mask` entry).
    pub fn add_mask(&mut self, atom: Atom) {
        self.masks.push(atom);
    }

    /// Remove a previously inherited masking atom (a `-atom` entry in a later
    /// layer).
    pub fn remove_mask(&mut self, atom: &Atom) {
        self.masks.retain(|m| m != atom);
    }

    /// Add an unmasking atom (a `package.unmask` entry).
    pub fn add_unmask(&mut self, atom: Atom) {
        self.unmasks.push(atom);
    }

    /// Whether a package is masked: matched by a mask and not cancelled by an
    /// unmask.
    pub fn is_masked(&self, pkg: &PackageRef<'_>) -> bool {
        let masked = self.masks.iter().any(|m| m.matches(pkg));
        if !masked {
            return false;
        }
        !self.unmasks.iter().any(|u| u.matches(pkg))
    }
}

/// Stacks `package.provided` entries and answers whether a package is provided
/// externally.
#[derive(Debug, Clone, Default)]
pub struct ProvidedManager {
    provided: Vec<Atom>,
}

impl ProvidedManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a provided atom.
    pub fn add(&mut self, atom: Atom) {
        self.provided.push(atom);
    }

    /// Whether a package is satisfied by a `package.provided` entry.
    pub fn is_provided(&self, pkg: &PackageRef<'_>) -> bool {
        self.provided.iter().any(|p| p.matches(pkg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moraine_atom::Atom;
    use moraine_common::Interner;
    use moraine_eapi::features_for_level;
    use moraine_version::Version;

    fn accepted(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stable_and_testing_keywords() {
        assert_eq!(
            accept_keywords(&["amd64".into()], &accepted(&["amd64"]), "amd64"),
            KeywordResult::Accepted
        );
        assert_eq!(
            accept_keywords(&["~amd64".into()], &accepted(&["amd64"]), "amd64"),
            KeywordResult::NeedsKeyword
        );
        assert_eq!(
            accept_keywords(&["~amd64".into()], &accepted(&["amd64", "~amd64"]), "amd64"),
            KeywordResult::Accepted
        );
        assert_eq!(
            accept_keywords(&[], &accepted(&["amd64"]), "amd64"),
            KeywordResult::NeedsDoubleStar
        );
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
    fn masking_and_unmasking() {
        let i = Interner::new();
        let f = features_for_level(8);
        let v = Version::parse("1.0").unwrap();
        let mut m = MaskManager::new();
        m.add_mask(Atom::parse("dev-libs/foo", f, &i).unwrap());
        assert!(m.is_masked(&pkg(&i, &v)));

        m.add_unmask(Atom::parse("dev-libs/foo", f, &i).unwrap());
        assert!(!m.is_masked(&pkg(&i, &v)));
    }

    #[test]
    fn negative_mask_removes_inherited() {
        let i = Interner::new();
        let f = features_for_level(8);
        let v = Version::parse("1.0").unwrap();
        let atom = Atom::parse("dev-libs/foo", f, &i).unwrap();
        let mut m = MaskManager::new();
        m.add_mask(atom.clone());
        m.remove_mask(&atom);
        assert!(!m.is_masked(&pkg(&i, &v)));
    }

    #[test]
    fn provided_predicate() {
        let i = Interner::new();
        let f = features_for_level(8);
        let v = Version::parse("1.0").unwrap();
        let mut p = ProvidedManager::new();
        p.add(Atom::parse("=dev-libs/foo-1.0", f, &i).unwrap());
        assert!(p.is_provided(&pkg(&i, &v)));
    }
}
