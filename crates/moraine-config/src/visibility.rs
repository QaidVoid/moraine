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
///
/// The accept decision mirrors `KeywordsManager._getMissingKeywords`: an ebuild
/// `*` keyword is an unconditional accept, an ebuild `~*` matches whenever any
/// accepted keyword is a testing (`~`) keyword, `*` accepts a stable keyword and
/// `~*` accepts a testing keyword, and `**` accepts anything. When no keyword
/// matches, the failure is reported as needing the testing keyword only when the
/// package carries the profile arch's `~arch` keyword (so that accepting `~arch`
/// would unmask it, mirroring `getmaskingstatus`); otherwise the package needs
/// `**`.
pub fn accept_keywords(
    keywords: &[String],
    accepted: &BTreeSet<String>,
    arch: &str,
) -> KeywordResult {
    let mut matched = false;
    let mut hasstable = false;
    let mut hastesting = false;
    for gp in keywords {
        if gp == "*" {
            matched = true;
            break;
        } else if gp == "~*" {
            hastesting = true;
            if accepted.iter().any(|x| x.starts_with('~')) {
                matched = true;
                break;
            }
        } else if accepted.contains(gp) {
            matched = true;
            break;
        } else if gp.starts_with('~') {
            hastesting = true;
        } else if !gp.starts_with('-') {
            hasstable = true;
        }
    }
    if !matched
        && ((hastesting && accepted.contains("~*"))
            || (hasstable && accepted.contains("*"))
            || accepted.contains("**"))
    {
        matched = true;
    }
    if matched {
        return KeywordResult::Accepted;
    }
    // Not accepted: the testing-keyword suggestion only unmasks when the package
    // carries the profile arch's `~arch` keyword and `arch` is otherwise
    // accepted; a cross-arch package (for example `~x86` on amd64) needs `**`.
    let testing_arch = format!("~{arch}");
    if keywords.contains(&testing_arch) && accepted.contains(arch) {
        KeywordResult::NeedsKeyword
    } else {
        KeywordResult::NeedsDoubleStar
    }
}

/// One `category` or `package` segment of an extended (`*`-wildcarded) cp
/// pattern such as `*/*`, `cat/*`, `*/pkg`, or a partial intra-segment wildcard
/// such as `games-*`, `foo*`, or `*-bin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    /// A bare `*` matching any name.
    Any,
    /// A literal name interned against the shared interner.
    Exact(Symbol),
    /// A partial wildcard segment, stored as the literal parts split on `*`;
    /// each `*` matches a `[^/]*` run within the single segment, mirroring
    /// `extended_cp_match`.
    Glob(Vec<Box<str>>),
}

fn seg_matches(seg: &Seg, sym: Symbol, interner: &moraine_common::Interner) -> bool {
    match seg {
        Seg::Any => true,
        Seg::Exact(s) => *s == sym,
        Seg::Glob(parts) => interner
            .resolve(sym)
            .map(|name| glob_match(parts, &name))
            .unwrap_or(false),
    }
}

/// Match a single segment's `*`-split literal `parts` against `name`. Each `*`
/// (the gap between two parts) matches any run of characters; the first part
/// anchors the prefix and the last anchors the suffix, with the middle parts
/// required in order.
fn glob_match(parts: &[Box<str>], name: &str) -> bool {
    if parts.len() == 1 {
        return parts[0].as_ref() == name;
    }
    let Some(rest) = name.strip_prefix(parts[0].as_ref()) else {
        return false;
    };
    let last = parts[parts.len() - 1].as_ref();
    if rest.len() < last.len() || !rest.ends_with(last) {
        return false;
    }
    let mut hay = &rest[..rest.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match hay.find(part.as_ref()) {
            Some(idx) => hay = &hay[idx + part.len()..],
            None => return false,
        }
    }
    true
}

/// A mask pattern: either a concrete atom or an extended cp wildcard. The
/// extended form matches whole-segment `*` wildcards (`*/*`, `cat/*`, `*/pkg`)
/// and partial intra-segment wildcards (`games-*/*`, `cat/foo*`); the literal
/// whole segments are interned so matching is a symbol comparison, and only a
/// partial glob segment resolves the candidate symbol against the interner.
#[derive(Debug, Clone)]
pub enum MaskPattern {
    /// A concrete versioned or unversioned atom.
    Atom(Box<Atom>),
    /// An extended cp wildcard `(category, package)`.
    Extended(Seg, Seg),
}

impl MaskPattern {
    /// Whether this pattern matches `pkg`. `interner` resolves the candidate's
    /// `category`/`package` symbols when (and only when) a partial glob segment
    /// is involved.
    pub fn matches(&self, pkg: &PackageRef<'_>, interner: &moraine_common::Interner) -> bool {
        match self {
            MaskPattern::Atom(atom) => atom.matches(pkg),
            MaskPattern::Extended(cat, pkg_seg) => {
                seg_matches(cat, pkg.category, interner)
                    && seg_matches(pkg_seg, pkg.package, interner)
            }
        }
    }
}

/// Parse one cp token (the text after any leading `-`) into a pattern, shared by
/// the mask, keyword, and license per-package stores.
///
/// A concrete atom is parsed against `features`; if that fails, the token is
/// tried as an extended `cat/pkg` wildcard. Whole-segment `*` becomes
/// [`Seg::Any`], a partial wildcard (for example `games-*` or `*-bin`) becomes
/// [`Seg::Glob`], and a plain literal becomes [`Seg::Exact`]. A token with a
/// slot or USE dependency is not a valid extended cp and yields `None`.
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
        if s.is_empty() {
            None
        } else if s == "*" {
            Some(Seg::Any)
        } else if s.contains('*') {
            Some(Seg::Glob(s.split('*').map(Box::from).collect()))
        } else {
            Some(Seg::Exact(interner.intern(s)))
        }
    };
    Some(MaskPattern::Extended(seg(cat)?, seg(pkg)?))
}

/// A specificity score for ordering per-package entries keyed by a
/// [`MaskPattern`] (more specific applied last). A concrete atom scores by its
/// version, slot, and repository qualifiers; an extended cp wildcard is the
/// least specific.
pub fn pattern_specificity(pattern: &MaskPattern) -> u32 {
    match pattern {
        MaskPattern::Atom(atom) => {
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
        MaskPattern::Extended(..) => 0,
    }
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
    fn applies_to(&self, pkg: &PackageRef<'_>, interner: &moraine_common::Interner) -> bool {
        if let Some(repo) = self.repo
            && pkg.repo != Some(repo)
        {
            return false;
        }
        self.pattern.matches(pkg, interner)
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
    /// cancelled by a standing unmask. `interner` resolves candidate symbols for
    /// partial glob matching.
    pub fn is_masked(&self, pkg: &PackageRef<'_>, interner: &moraine_common::Interner) -> bool {
        matches!(self.reason(pkg, interner), MaskReason::HardMasked(_))
    }

    /// The structured masking reason, naming the responsible mask token when the
    /// package is hard-masked. The most recently stacked applicable mask is
    /// reported.
    pub fn reason(&self, pkg: &PackageRef<'_>, interner: &moraine_common::Interner) -> MaskReason {
        let Some(entry) = self
            .masks
            .iter()
            .rev()
            .find(|m| m.applies_to(pkg, interner))
        else {
            return MaskReason::Visible;
        };
        if self.unmasks.iter().any(|u| u.matches(pkg, interner)) {
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
        // ACCEPT_KEYWORDS = {~amd64} accepts ~amd64 but not stable amd64. A
        // stable-only package is not unmasked by `~amd64`, so the suggestion is
        // `**` (NeedsDoubleStar), mirroring getmaskingstatus.
        assert_eq!(
            accept_keywords(&["amd64".into()], &accepted(&["~amd64"]), "amd64"),
            KeywordResult::NeedsDoubleStar
        );
        assert_eq!(
            accept_keywords(&["~amd64".into()], &accepted(&["~amd64"]), "amd64"),
            KeywordResult::Accepted
        );
    }

    #[test]
    fn ebuild_star_is_unconditional_accept() {
        // An ebuild `*` keyword accepts even when `*` is not literally accepted.
        assert_eq!(
            accept_keywords(&["*".into()], &accepted(&["amd64"]), "amd64"),
            KeywordResult::Accepted
        );
    }

    #[test]
    fn ebuild_tilde_star_matches_any_testing_keyword() {
        // An ebuild `~*` accepts when any accepted keyword is a testing keyword.
        assert_eq!(
            accept_keywords(&["~*".into()], &accepted(&["amd64", "~amd64"]), "amd64"),
            KeywordResult::Accepted
        );
        // With no accepted testing keyword, `~*` does not match on its own.
        assert_eq!(
            accept_keywords(&["~*".into()], &accepted(&["amd64"]), "amd64"),
            KeywordResult::NeedsDoubleStar
        );
    }

    #[test]
    fn cross_arch_package_needs_double_star() {
        // A `~x86` package on an amd64 profile is not unmasked by `~amd64`, so it
        // must report the `**` outcome rather than a non-functional keyword.
        assert_eq!(
            accept_keywords(&["~x86".into()], &accepted(&["amd64"]), "amd64"),
            KeywordResult::NeedsDoubleStar
        );
        // A `~amd64` package on the same profile reports the keyword outcome.
        assert_eq!(
            accept_keywords(&["~amd64".into()], &accepted(&["amd64"]), "amd64"),
            KeywordResult::NeedsKeyword
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
        assert!(masked.is_masked(&pkg(&i, &v), &i));

        let mut b = MaskBuilder::new();
        b.push("dev-libs/foo", pat("dev-libs/foo", &i), None);
        b.add_standing_unmask(pat("dev-libs/foo", &i));
        assert!(!b.build().is_masked(&pkg(&i, &v), &i));
    }

    #[test]
    fn negative_mask_pops_inherited() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut b = MaskBuilder::new();
        b.push("dev-libs/foo", pat("dev-libs/foo", &i), None);
        b.pop("dev-libs/foo");
        assert!(!b.build().is_masked(&pkg(&i, &v), &i));
    }

    #[test]
    fn star_clears_accumulated_masks() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut b = MaskBuilder::new();
        b.push("dev-libs/foo", pat("dev-libs/foo", &i), None);
        b.clear();
        b.push("dev-libs/other", pat("dev-libs/other", &i), None);
        assert!(!b.build().is_masked(&pkg(&i, &v), &i));
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
        assert!(m.is_masked(&pkg_in(&i, &v, Some("gentoo")), &i));
        assert!(!m.is_masked(&pkg_in(&i, &v, Some("overlay")), &i));
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
        assert!(!b.build().is_masked(&pkg_in(&i, &v, Some("gentoo")), &i));
    }

    #[test]
    fn extended_wildcard_masks() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        for token in ["*/*", "dev-libs/*", "*/foo"] {
            let mut b = MaskBuilder::new();
            b.push(token, pat(token, &i), None);
            assert!(
                b.build().is_masked(&pkg(&i, &v), &i),
                "pattern {token} should mask dev-libs/foo"
            );
        }
        // A non-matching wildcard leaves the package visible.
        let mut b = MaskBuilder::new();
        b.push("dev-util/*", pat("dev-util/*", &i), None);
        assert!(!b.build().is_masked(&pkg(&i, &v), &i));
    }

    #[test]
    fn partial_intra_segment_wildcard_masks() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        // The candidate is dev-python/foo-bin (package name `foo-bin`).
        let bin = PackageRef {
            category: i.intern("dev-python"),
            package: i.intern("foo-bin"),
            version: &v,
            slot: None,
            subslot: None,
            repo: None,
        };
        for token in ["dev-python/*-bin", "dev-python/foo*", "*/foo-bin"] {
            let mut b = MaskBuilder::new();
            b.push(token, pat(token, &i), None);
            assert!(
                b.build().is_masked(&bin, &i),
                "pattern {token} should mask dev-python/foo-bin"
            );
        }
        // A partial wildcard category (`games-*`) matches by prefix.
        let game = PackageRef {
            category: i.intern("games-rpg"),
            package: i.intern("nethack"),
            version: &v,
            slot: None,
            subslot: None,
            repo: None,
        };
        let mut b = MaskBuilder::new();
        b.push("games-*/*", pat("games-*/*", &i), None);
        let games_mask = b.build();
        assert!(games_mask.is_masked(&game, &i));
        // The same `games-*` does not match a non-`games-` category.
        assert!(!games_mask.is_masked(&pkg(&i, &v), &i));
        // A partial wildcard that does not match leaves the package visible.
        let mut b = MaskBuilder::new();
        b.push("dev-python/*-doc", pat("dev-python/*-doc", &i), None);
        assert!(!b.build().is_masked(&bin, &i));
    }

    #[test]
    fn whole_segment_wildcards_still_match() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        for token in ["*/*", "dev-libs/*", "*/foo"] {
            let mut b = MaskBuilder::new();
            b.push(token, pat(token, &i), None);
            assert!(b.build().is_masked(&pkg(&i, &v), &i), "{token}");
        }
    }

    #[test]
    fn mask_reason_names_the_token() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut b = MaskBuilder::new();
        b.push(">=dev-libs/foo-1", pat(">=dev-libs/foo-1", &i), None);
        assert_eq!(
            b.build().reason(&pkg(&i, &v), &i),
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
