//! Per-package keyword configuration: profile `package.keywords` (which modifies
//! a package's `KEYWORDS`) and the per-package accepted keywords from profile and
//! user `package.accept_keywords` (plus the deprecated user `package.keywords`),
//! mirroring Portage's `KeywordsManager`.

use moraine_atom::PackageRef;
use moraine_common::Interner;

use crate::visibility::{MaskPattern, pattern_specificity};

/// A pattern-keyed list of keyword tokens. The pattern is either a concrete atom
/// or an extended cp wildcard (`*/*`, `games-*/*`), so a wildcard line such as
/// `*/* ~amd64` applies to every matching candidate.
#[derive(Debug, Clone)]
struct KeywordEntry {
    pattern: MaskPattern,
    tokens: Vec<String>,
}

/// Resolves a package's stacked `KEYWORDS` and its per-package accepted keywords.
#[derive(Debug, Clone, Default)]
pub struct KeywordsManager {
    /// Profile `package.keywords`: incremental `KEYWORDS` modifications.
    profile_keywords: Vec<KeywordEntry>,
    /// Per-package accepted keywords (`package.accept_keywords` and the
    /// deprecated user `package.keywords`).
    pkeywords: Vec<KeywordEntry>,
}

impl KeywordsManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a profile `package.keywords` entry (a `KEYWORDS` modification).
    pub fn add_profile_keywords(&mut self, pattern: MaskPattern, tokens: Vec<String>) {
        self.profile_keywords.push(KeywordEntry { pattern, tokens });
        self.profile_keywords
            .sort_by_key(|e| pattern_specificity(&e.pattern));
    }

    /// Add a per-package accepted-keywords entry.
    pub fn add_pkeywords(&mut self, pattern: MaskPattern, tokens: Vec<String>) {
        self.pkeywords.push(KeywordEntry { pattern, tokens });
        self.pkeywords
            .sort_by_key(|e| pattern_specificity(&e.pattern));
    }

    /// The package's effective `KEYWORDS`: the ebuild keywords (with a leading
    /// `-*` dropped) with matched profile `package.keywords` stacked
    /// incrementally (a `-kw` removes, `-*` clears), in atom-specificity order.
    pub fn stacked_keywords(
        &self,
        pkg: &PackageRef<'_>,
        ebuild_keywords: &[String],
        interner: &Interner,
    ) -> Vec<String> {
        let mut acc: Vec<String> = ebuild_keywords
            .iter()
            .filter(|k| *k != "-*")
            .cloned()
            .collect();
        for entry in &self.profile_keywords {
            if entry.pattern.matches(pkg, interner) {
                for token in &entry.tokens {
                    if token == "-*" {
                        acc.clear();
                    } else if let Some(rest) = token.strip_prefix('-') {
                        acc.retain(|k| k != rest);
                    } else if !acc.iter().any(|k| k == token) {
                        acc.push(token.clone());
                    }
                }
            }
        }
        acc
    }

    /// The per-package accepted keywords matching `pkg`, in atom-specificity
    /// order. A bare entry (no explicit keyword) contributes one empty token,
    /// which the keyword acceptor expands to the per-arch testing default.
    pub fn pkeywords(&self, pkg: &PackageRef<'_>, interner: &Interner) -> Vec<String> {
        let mut out = Vec::new();
        for entry in &self.pkeywords {
            if entry.pattern.matches(pkg, interner) {
                if entry.tokens.is_empty() {
                    out.push(String::new());
                } else {
                    out.extend(entry.tokens.iter().cloned());
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::visibility::parse_mask_pattern;
    use moraine_common::Interner;
    use moraine_version::Version;

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

    fn pat(i: &Interner, text: &str) -> MaskPattern {
        parse_mask_pattern(text, i, moraine_eapi::PERMISSIVE).unwrap()
    }

    #[test]
    fn profile_keywords_stack_incrementally() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut m = KeywordsManager::new();
        m.add_profile_keywords(pat(&i, "dev-libs/foo"), vec!["~arm64".to_owned()]);
        let kw = m.stacked_keywords(&pkg(&i, &v), &["amd64".to_owned()], &i);
        assert!(kw.contains(&"amd64".to_owned()) && kw.contains(&"~arm64".to_owned()));

        // A `-kw` removes and `-*` clears.
        let mut m2 = KeywordsManager::new();
        m2.add_profile_keywords(pat(&i, "dev-libs/foo"), vec!["-amd64".to_owned()]);
        let kw2 = m2.stacked_keywords(&pkg(&i, &v), &["amd64".to_owned(), "~amd64".to_owned()], &i);
        assert_eq!(kw2, vec!["~amd64".to_owned()]);
    }

    #[test]
    fn ebuild_minus_star_is_dropped() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let m = KeywordsManager::new();
        let kw = m.stacked_keywords(&pkg(&i, &v), &["-*".to_owned(), "~amd64".to_owned()], &i);
        assert_eq!(kw, vec!["~amd64".to_owned()]);
    }

    #[test]
    fn pkeywords_match_and_bare_is_empty_token() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut m = KeywordsManager::new();
        m.add_pkeywords(pat(&i, "dev-libs/foo"), vec!["~amd64".to_owned()]);
        m.add_pkeywords(pat(&i, "dev-libs/bare"), Vec::new());
        assert_eq!(m.pkeywords(&pkg(&i, &v), &i), vec!["~amd64".to_owned()]);

        let bare = PackageRef {
            package: i.intern("bare"),
            ..pkg(&i, &v)
        };
        assert_eq!(m.pkeywords(&bare, &i), vec![String::new()]);
    }

    #[test]
    fn extended_wildcard_pkeywords_match_across_categories() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut m = KeywordsManager::new();
        // `*/* ~amd64` accepts the testing keyword for every candidate.
        m.add_pkeywords(pat(&i, "*/*"), vec!["~amd64".to_owned()]);
        assert_eq!(m.pkeywords(&pkg(&i, &v), &i), vec!["~amd64".to_owned()]);
        let other = PackageRef {
            category: i.intern("games-rpg"),
            package: i.intern("nethack"),
            ..pkg(&i, &v)
        };
        assert_eq!(m.pkeywords(&other, &i), vec!["~amd64".to_owned()]);
    }
}
