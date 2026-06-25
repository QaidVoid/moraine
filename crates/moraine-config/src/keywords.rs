//! Per-package keyword configuration: profile `package.keywords` (which modifies
//! a package's `KEYWORDS`) and the per-package accepted keywords from profile and
//! user `package.accept_keywords` (plus the deprecated user `package.keywords`),
//! mirroring Portage's `KeywordsManager`.

use moraine_atom::{Atom, PackageRef};

/// An atom-keyed list of keyword tokens.
#[derive(Debug, Clone)]
struct KeywordEntry {
    atom: Atom,
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
    pub fn add_profile_keywords(&mut self, atom: Atom, tokens: Vec<String>) {
        self.profile_keywords.push(KeywordEntry { atom, tokens });
        self.profile_keywords.sort_by_key(|e| specificity(&e.atom));
    }

    /// Add a per-package accepted-keywords entry.
    pub fn add_pkeywords(&mut self, atom: Atom, tokens: Vec<String>) {
        self.pkeywords.push(KeywordEntry { atom, tokens });
        self.pkeywords.sort_by_key(|e| specificity(&e.atom));
    }

    /// The package's effective `KEYWORDS`: the ebuild keywords (with a leading
    /// `-*` dropped) with matched profile `package.keywords` stacked
    /// incrementally (a `-kw` removes, `-*` clears), in atom-specificity order.
    pub fn stacked_keywords(
        &self,
        pkg: &PackageRef<'_>,
        ebuild_keywords: &[String],
    ) -> Vec<String> {
        let mut acc: Vec<String> = ebuild_keywords
            .iter()
            .filter(|k| *k != "-*")
            .cloned()
            .collect();
        for entry in &self.profile_keywords {
            if entry.atom.matches(pkg) {
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
    pub fn pkeywords(&self, pkg: &PackageRef<'_>) -> Vec<String> {
        let mut out = Vec::new();
        for entry in &self.pkeywords {
            if entry.atom.matches(pkg) {
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

/// A specificity score for ordering per-package entries (more specific last).
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

    fn atom(i: &Interner, text: &str) -> Atom {
        Atom::parse(text, moraine_eapi::PERMISSIVE, i).unwrap()
    }

    #[test]
    fn profile_keywords_stack_incrementally() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut m = KeywordsManager::new();
        m.add_profile_keywords(atom(&i, "dev-libs/foo"), vec!["~arm64".to_owned()]);
        let kw = m.stacked_keywords(&pkg(&i, &v), &["amd64".to_owned()]);
        assert!(kw.contains(&"amd64".to_owned()) && kw.contains(&"~arm64".to_owned()));

        // A `-kw` removes and `-*` clears.
        let mut m2 = KeywordsManager::new();
        m2.add_profile_keywords(atom(&i, "dev-libs/foo"), vec!["-amd64".to_owned()]);
        let kw2 = m2.stacked_keywords(&pkg(&i, &v), &["amd64".to_owned(), "~amd64".to_owned()]);
        assert_eq!(kw2, vec!["~amd64".to_owned()]);
    }

    #[test]
    fn ebuild_minus_star_is_dropped() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let m = KeywordsManager::new();
        let kw = m.stacked_keywords(&pkg(&i, &v), &["-*".to_owned(), "~amd64".to_owned()]);
        assert_eq!(kw, vec!["~amd64".to_owned()]);
    }

    #[test]
    fn pkeywords_match_and_bare_is_empty_token() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut m = KeywordsManager::new();
        m.add_pkeywords(atom(&i, "dev-libs/foo"), vec!["~amd64".to_owned()]);
        m.add_pkeywords(atom(&i, "dev-libs/bare"), Vec::new());
        assert_eq!(m.pkeywords(&pkg(&i, &v)), vec!["~amd64".to_owned()]);

        let bare = PackageRef {
            package: i.intern("bare"),
            ..pkg(&i, &v)
        };
        assert_eq!(m.pkeywords(&bare), vec![String::new()]);
    }
}
