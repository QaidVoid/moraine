//! Package visibility: masking, keyword acceptance, and `package.provided`.

use std::collections::{BTreeSet, HashMap};

use moraine_atom::{Atom, PackageRef};
use moraine_common::Symbol;

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
        } else if accepted.contains("*") {
            // A stable keyword is accepted only by exact membership, `*`, or
            // `**`; a `~arch` in ACCEPT_KEYWORDS does not by itself accept the
            // stable `arch` keyword.
            return KeywordResult::Accepted;
        }
    }
    if !saw_real {
        return KeywordResult::NeedsDoubleStar;
    }
    KeywordResult::NeedsKeyword
}

/// One `category` or `package` segment of an extended (`*`-wildcarded) cp
/// pattern such as `*/*`, `cat/*`, or `*/pkg`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    /// A bare `*` matching any name.
    Any,
    /// A literal name interned against the shared interner.
    Exact(Symbol),
}

fn seg_matches(seg: &Seg, sym: Symbol) -> bool {
    match seg {
        Seg::Any => true,
        Seg::Exact(s) => *s == sym,
    }
}

/// A mask pattern: either a concrete atom or an extended cp wildcard. The
/// extended form matches whole-segment `*` wildcards (`*/*`, `cat/*`, `*/pkg`);
/// the literal segments are interned so matching is a symbol comparison and
/// needs no interner at query time.
#[derive(Debug, Clone)]
pub enum MaskPattern {
    /// A concrete versioned or unversioned atom.
    Atom(Box<Atom>),
    /// An extended cp wildcard `(category, package)`.
    Extended(Seg, Seg),
}

impl MaskPattern {
    fn matches(&self, pkg: &PackageRef<'_>) -> bool {
        match self {
            MaskPattern::Atom(atom) => atom.matches(pkg),
            MaskPattern::Extended(cat, pkg_seg) => {
                seg_matches(cat, pkg.category) && seg_matches(pkg_seg, pkg.package)
            }
        }
    }
}

/// Parse one mask token (the text after any leading `-`) into a pattern.
///
/// A concrete atom is parsed against `features`; if that fails, the token is
/// tried as an extended `cat/pkg` wildcard with whole-segment `*`. A partial
/// wildcard (for example `foo*`) or a token with a slot or USE dependency is not
/// a valid extended mask and yields `None`.
pub fn parse_mask_pattern(
    text: &str,
    interner: &moraine_common::Interner,
    features: moraine_eapi::EapiFeatures,
) -> Option<MaskPattern> {
    if let Ok(atom) = Atom::parse(text, features, interner) {
        return Some(MaskPattern::Atom(Box::new(atom)));
    }
    let (cat, pkg) = text.split_once('/')?;
    if pkg.contains([':', '[']) || cat.contains([':', '[']) {
        return None;
    }
    let seg = |s: &str| -> Option<Seg> {
        if s == "*" {
            Some(Seg::Any)
        } else if s.contains('*') {
            None
        } else {
            Some(Seg::Exact(interner.intern(s)))
        }
    };
    Some(MaskPattern::Extended(seg(cat)?, seg(pkg)?))
}

/// One stacked mask entry: its pattern, the original source token (for an
/// explanation), and an optional `::repo` scope restricting it to candidates
/// from that repository.
#[derive(Debug, Clone)]
struct MaskEntry {
    pattern: MaskPattern,
    source: String,
    repo: Option<Symbol>,
}

impl MaskEntry {
    fn applies_to(&self, pkg: &PackageRef<'_>) -> bool {
        if let Some(repo) = self.repo
            && pkg.repo != Some(repo)
        {
            return false;
        }
        self.pattern.matches(pkg)
    }
}

/// Builds a [`MaskManager`] by stacking mask layers incrementally, mirroring
/// Portage's `stack_lists`: a plain token pushes a mask, a `-token` pops the
/// matching prior mask, and `-*` clears the accumulator. Standing unmasks (from
/// `/etc/portage`) are kept separate and cancel a match from any layer.
#[derive(Debug, Default)]
pub struct MaskBuilder {
    order: Vec<String>,
    entries: HashMap<String, MaskEntry>,
    unmasks: Vec<MaskPattern>,
}

impl MaskBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    fn key(token: &str, repo: Option<&str>) -> String {
        match repo {
            Some(r) => format!("{token}::{r}"),
            None => token.to_owned(),
        }
    }

    /// Push a mask, keyed by its source token (and `::repo` when scoped). A
    /// repeated key keeps its original position, matching `stack_lists`.
    pub fn push(&mut self, token: &str, pattern: MaskPattern, repo: Option<(&str, Symbol)>) {
        let key = Self::key(token, repo.map(|(name, _)| name));
        if !self.entries.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.entries.insert(
            key,
            MaskEntry {
                pattern,
                source: token.to_owned(),
                repo: repo.map(|(_, sym)| sym),
            },
        );
    }

    /// Pop a previously stacked mask by its source token. The match ignores the
    /// `::repo` scope, so a bare `-cat/pkg` removes both `cat/pkg` and any
    /// `cat/pkg::repo`, mirroring `stack_lists`'s `ignore_repo` removal.
    pub fn pop(&mut self, token: &str) {
        let victims: Vec<String> = self
            .order
            .iter()
            .filter(|k| k.split("::").next() == Some(token))
            .cloned()
            .collect();
        for key in victims {
            self.entries.remove(&key);
            self.order.retain(|k| k != &key);
        }
    }

    /// Clear all masks accumulated so far (a `-*` line).
    pub fn clear(&mut self) {
        self.order.clear();
        self.entries.clear();
    }

    /// Add a standing unmask that cancels a matching mask from any layer.
    pub fn add_standing_unmask(&mut self, pattern: MaskPattern) {
        self.unmasks.push(pattern);
    }

    /// Finalize into a queryable [`MaskManager`].
    pub fn build(self) -> MaskManager {
        let masks = self
            .order
            .iter()
            .filter_map(|k| self.entries.get(k).cloned())
            .collect();
        MaskManager {
            masks,
            unmasks: self.unmasks,
        }
    }
}

/// Why a package fails or passes the masking check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaskReason {
    /// The package is not masked.
    Visible,
    /// The package is hard-masked; the string is the responsible mask token.
    HardMasked(String),
}

/// Stacks `package.mask` / `package.unmask` and answers whether a package is
/// masked. Build one with [`MaskBuilder`].
#[derive(Debug, Clone, Default)]
pub struct MaskManager {
    masks: Vec<MaskEntry>,
    unmasks: Vec<MaskPattern>,
}

impl MaskManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a package is masked: matched by a mask of applicable scope and not
    /// cancelled by a standing unmask.
    pub fn is_masked(&self, pkg: &PackageRef<'_>) -> bool {
        matches!(self.reason(pkg), MaskReason::HardMasked(_))
    }

    /// The structured masking reason, naming the responsible mask token when the
    /// package is hard-masked. The most recently stacked applicable mask is
    /// reported.
    pub fn reason(&self, pkg: &PackageRef<'_>) -> MaskReason {
        let Some(entry) = self.masks.iter().rev().find(|m| m.applies_to(pkg)) else {
            return MaskReason::Visible;
        };
        if self.unmasks.iter().any(|u| u.matches(pkg)) {
            return MaskReason::Visible;
        }
        MaskReason::HardMasked(entry.source.clone())
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

    #[test]
    fn stable_not_accepted_by_testing_keyword_only() {
        // ACCEPT_KEYWORDS = {~amd64} accepts ~amd64 but not stable amd64.
        assert_eq!(
            accept_keywords(&["amd64".into()], &accepted(&["~amd64"]), "amd64"),
            KeywordResult::NeedsKeyword
        );
        assert_eq!(
            accept_keywords(&["~amd64".into()], &accepted(&["~amd64"]), "amd64"),
            KeywordResult::Accepted
        );
    }

    fn pkg<'a>(i: &Interner, v: &'a Version) -> PackageRef<'a> {
        pkg_in(i, v, None)
    }

    fn pkg_in<'a>(i: &Interner, v: &'a Version, repo: Option<&str>) -> PackageRef<'a> {
        PackageRef {
            category: i.intern("dev-libs"),
            package: i.intern("foo"),
            version: v,
            slot: None,
            subslot: None,
            repo: repo.map(|r| i.intern(r)),
        }
    }

    fn pat(text: &str, i: &Interner) -> MaskPattern {
        parse_mask_pattern(text, i, features_for_level(8)).unwrap()
    }

    #[test]
    fn masking_and_unmasking() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut b = MaskBuilder::new();
        b.push("dev-libs/foo", pat("dev-libs/foo", &i), None);
        let masked = b.build();
        assert!(masked.is_masked(&pkg(&i, &v)));

        let mut b = MaskBuilder::new();
        b.push("dev-libs/foo", pat("dev-libs/foo", &i), None);
        b.add_standing_unmask(pat("dev-libs/foo", &i));
        assert!(!b.build().is_masked(&pkg(&i, &v)));
    }

    #[test]
    fn negative_mask_pops_inherited() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut b = MaskBuilder::new();
        b.push("dev-libs/foo", pat("dev-libs/foo", &i), None);
        b.pop("dev-libs/foo");
        assert!(!b.build().is_masked(&pkg(&i, &v)));
    }

    #[test]
    fn star_clears_accumulated_masks() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut b = MaskBuilder::new();
        b.push("dev-libs/foo", pat("dev-libs/foo", &i), None);
        b.clear();
        b.push("dev-libs/other", pat("dev-libs/other", &i), None);
        assert!(!b.build().is_masked(&pkg(&i, &v)));
    }

    #[test]
    fn repo_scoped_mask_only_applies_to_that_repo() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let gentoo = i.intern("gentoo");
        let mut b = MaskBuilder::new();
        b.push(
            "dev-libs/foo",
            pat("dev-libs/foo", &i),
            Some(("gentoo", gentoo)),
        );
        let m = b.build();
        assert!(m.is_masked(&pkg_in(&i, &v, Some("gentoo"))));
        assert!(!m.is_masked(&pkg_in(&i, &v, Some("overlay"))));
    }

    #[test]
    fn bare_unmask_cancels_repo_scoped_mask() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let gentoo = i.intern("gentoo");
        let mut b = MaskBuilder::new();
        b.push(
            "dev-libs/foo",
            pat("dev-libs/foo", &i),
            Some(("gentoo", gentoo)),
        );
        b.add_standing_unmask(pat("dev-libs/foo", &i));
        assert!(!b.build().is_masked(&pkg_in(&i, &v, Some("gentoo"))));
    }

    #[test]
    fn extended_wildcard_masks() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        for token in ["*/*", "dev-libs/*", "*/foo"] {
            let mut b = MaskBuilder::new();
            b.push(token, pat(token, &i), None);
            assert!(
                b.build().is_masked(&pkg(&i, &v)),
                "pattern {token} should mask dev-libs/foo"
            );
        }
        // A non-matching wildcard leaves the package visible.
        let mut b = MaskBuilder::new();
        b.push("dev-util/*", pat("dev-util/*", &i), None);
        assert!(!b.build().is_masked(&pkg(&i, &v)));
    }

    #[test]
    fn mask_reason_names_the_token() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut b = MaskBuilder::new();
        b.push(">=dev-libs/foo-1", pat(">=dev-libs/foo-1", &i), None);
        assert_eq!(
            b.build().reason(&pkg(&i, &v)),
            MaskReason::HardMasked(">=dev-libs/foo-1".to_owned())
        );
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
