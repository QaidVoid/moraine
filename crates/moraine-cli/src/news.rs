//! GLEP 42 news item reading, relevance, and display.
//!
//! News items live under each repository's `metadata/news` directory, one
//! directory per item holding a `<name>.<lang>.txt` body. The header carries
//! `News-Item-Format`, `Display-If-Installed`, `Display-If-Profile`, and
//! `Display-If-Keyword`. This module parses items, validates them by format,
//! evaluates relevance against the environment with full atom and repo-relative
//! profile semantics, and reads bodies with a language fallback. State writing
//! lives in [`crate::news_state`].

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use miette::Diagnostic;
use moraine_atom::{Atom, PackageRef};
use moraine_common::Interner;
use moraine_eapi::{EapiFeatures, features_for};
use moraine_version::Version;
use thiserror::Error;
use tracing::instrument;

/// Errors from reading news.
#[derive(Debug, Error, Diagnostic)]
pub enum NewsError {
    /// A news directory or file could not be read.
    #[error("failed to read news at `{path}`")]
    #[diagnostic(code(moraine::news::io))]
    Io {
        /// The path being read.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

/// A GLEP 42 display restriction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restriction {
    /// The item is shown only if the atom is installed.
    IfInstalled(String),
    /// The item is shown only if the active profile matches the path.
    IfProfile(String),
    /// The item is shown only if the keyword is accepted.
    IfKeyword(String),
}

/// An installed package, for full-atom `Display-If-Installed` matching.
#[derive(Debug, Clone)]
pub struct InstalledPkg {
    /// The package category.
    pub category: String,
    /// The package name.
    pub package: String,
    /// The installed version.
    pub version: Version,
    /// The installed slot.
    pub slot: String,
    /// The installed sub-slot, if any.
    pub subslot: Option<String>,
}

/// A parsed news item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewsItem {
    /// The item identifier, taken from its directory name.
    pub name: String,
    /// The `Title:` header.
    pub title: String,
    /// The `News-Item-Format:` header value (for example `1.0` or `2.0`).
    pub format: String,
    /// The display restrictions declared by the item.
    pub restrictions: Vec<Restriction>,
}

/// The environment a news item's relevance is evaluated against.
#[derive(Debug, Clone, Default)]
pub struct NewsEnv {
    /// The installed packages, for full-atom matching.
    pub installed: Vec<InstalledPkg>,
    /// The active profile path, repository-relative.
    pub profile: String,
    /// The system arch keyword, for example `amd64`.
    pub arch: String,
}

/// The EAPI a news item's `Display-If-Installed` atoms are validated under,
/// gated by the item format: `0` for `1.*`, `5` for `2.*`, `None` otherwise.
fn format_eapi(format: &str) -> Option<&'static str> {
    if format.starts_with("1.") {
        Some("0")
    } else if format.starts_with("2.") {
        Some("5")
    } else {
        None
    }
}

impl NewsItem {
    /// Parse a news item from its header text and directory name.
    pub fn parse(name: &str, header: &str) -> NewsItem {
        let mut title = String::new();
        let mut format = String::new();
        let mut restrictions = Vec::new();
        for line in header.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "Title" => title = value.to_owned(),
                "News-Item-Format" => format = value.to_owned(),
                "Display-If-Installed" => {
                    restrictions.push(Restriction::IfInstalled(value.to_owned()))
                }
                "Display-If-Profile" => restrictions.push(Restriction::IfProfile(value.to_owned())),
                "Display-If-Keyword" => restrictions.push(Restriction::IfKeyword(value.to_owned())),
                _ => {}
            }
        }
        NewsItem {
            name: name.to_owned(),
            title,
            format,
            restrictions,
        }
    }

    /// Whether the item is structurally valid: its format matches `[12].*` and
    /// every restriction is well-formed under that format's EAPI. An invalid item
    /// is skipped entirely, mirroring `news.py`'s validation.
    pub fn is_valid(&self) -> bool {
        let Some(eapi) = format_eapi(&self.format) else {
            return false;
        };
        let features = features_for(eapi);
        let interner = Interner::new();
        self.restrictions.iter().all(|r| match r {
            Restriction::IfInstalled(atom) => Atom::parse(atom, features, &interner).is_ok(),
            Restriction::IfProfile(path) => !path.is_empty(),
            Restriction::IfKeyword(_) => true,
        })
    }

    /// Whether the item is relevant in the given environment.
    ///
    /// Restrictions of the same kind are alternatives; restrictions of different
    /// kinds are joint requirements. An item with no restrictions is always
    /// relevant.
    pub fn is_relevant(&self, env: &NewsEnv) -> bool {
        let features = features_for(format_eapi(&self.format).unwrap_or("0"));
        let mut installed = Vec::new();
        let mut profiles = Vec::new();
        let mut keywords = Vec::new();
        for r in &self.restrictions {
            match r {
                Restriction::IfInstalled(a) => installed.push(a),
                Restriction::IfProfile(p) => profiles.push(p),
                Restriction::IfKeyword(k) => keywords.push(k),
            }
        }

        let installed_ok = installed.is_empty()
            || installed
                .iter()
                .any(|atom| installed_matches(&env.installed, atom, features));
        let profile_ok = profiles.is_empty()
            || profiles
                .iter()
                .any(|p| profile_matches(&env.profile, p, &self.format));
        let keyword_ok =
            keywords.is_empty() || keywords.iter().any(|k| keyword_matches(&env.arch, k));

        installed_ok && profile_ok && keyword_ok
    }
}

/// Whether any installed package satisfies the `Display-If-Installed` atom, with
/// full version-operator and slot semantics. An unparseable atom matches nothing.
fn installed_matches(installed: &[InstalledPkg], atom: &str, features: EapiFeatures) -> bool {
    let interner = Interner::new();
    let Ok(parsed) = Atom::parse(atom, features, &interner) else {
        return false;
    };
    installed.iter().any(|pkg| {
        let pref = PackageRef {
            category: interner.intern(&pkg.category),
            package: interner.intern(&pkg.package),
            version: &pkg.version,
            slot: Some(interner.intern(&pkg.slot)),
            subslot: pkg.subslot.as_deref().map(|s| interner.intern(s)),
            repo: None,
        };
        parsed.matches(&pref)
    })
}

/// Match a `Display-If-Profile` restriction against the repo-relative profile:
/// format `2.*` allows a trailing `/*` prefix match, every other format requires
/// exact equality, mirroring `DisplayProfileRestriction.checkRestriction`.
fn profile_matches(profile: &str, restriction: &str, format: &str) -> bool {
    if format.starts_with("2.")
        && let Some(prefix) = restriction.strip_suffix("/*")
    {
        return profile == prefix || profile.starts_with(&format!("{prefix}/"));
    }
    profile == restriction
}

/// Whether the arch satisfies a `Display-If-Keyword` restriction.
fn keyword_matches(arch: &str, keyword: &str) -> bool {
    let bare = keyword.trim_start_matches(['~', '-']);
    bare == arch
}

/// Read and evaluate unread, relevant news for one repository.
///
/// `news_dir` is the repository's `metadata/news` directory; `unread` is the set
/// of item names the per-repository state reports as unread; `lang` selects the
/// body language with an `en` fallback. Returns only items that are valid,
/// unread, and relevant. The unread state is read, never written.
#[instrument(skip(news_dir, env, unread))]
pub fn unread_relevant(
    news_dir: &Path,
    env: &NewsEnv,
    unread: &BTreeSet<String>,
    lang: &str,
) -> Result<Vec<NewsItem>, NewsError> {
    if !news_dir.exists() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    let entries = std::fs::read_dir(news_dir).map_err(|source| NewsError::Io {
        path: news_dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| NewsError::Io {
            path: news_dir.to_path_buf(),
            source,
        })?;
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !unread.contains(&name) {
            continue;
        }
        let Some(header) = read_header(&entry.path(), &name, lang)? else {
            continue;
        };
        let item = NewsItem::parse(&name, &header);
        if item.is_valid() && item.is_relevant(env) {
            items.push(item);
        }
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(items)
}

/// Read the header (text up to the first blank line) of a news item body,
/// preferring the `<name>.<lang>.txt` body and falling back to `<name>.en.txt`.
fn read_header(item_dir: &Path, name: &str, lang: &str) -> Result<Option<String>, NewsError> {
    let mut body = item_dir.join(format!("{name}.{lang}.txt"));
    if !body.exists() {
        body = item_dir.join(format!("{name}.en.txt"));
    }
    if !body.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&body).map_err(|source| NewsError::Io {
        path: body.clone(),
        source,
    })?;
    let header = text.split("\n\n").next().unwrap_or("").to_owned();
    Ok(Some(header))
}

/// Render a news summary for display.
///
/// Returns an empty string when there is nothing unread, so the caller emits no
/// news section in that case.
pub fn render_news(items: &[NewsItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Important: {} unread news item{} relevant to your system:",
        items.len(),
        if items.len() == 1 { "" } else { "s" }
    );
    for item in items {
        let _ = writeln!(out, "  - {} ({})", item.title, item.name);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(specs: &[&str]) -> Vec<InstalledPkg> {
        specs
            .iter()
            .map(|s| {
                // `category/package-version[:slot]`.
                let (head, slot) = match s.split_once(':') {
                    Some((h, sl)) => (h, sl.to_owned()),
                    None => (*s, "0".to_owned()),
                };
                let (cp, version) = head.rsplit_once('-').unwrap();
                let (category, package) = cp.split_once('/').unwrap();
                InstalledPkg {
                    category: category.to_owned(),
                    package: package.to_owned(),
                    version: Version::parse(version).unwrap(),
                    slot,
                    subslot: None,
                }
            })
            .collect()
    }

    fn env(specs: &[&str], profile: &str, arch: &str) -> NewsEnv {
        NewsEnv {
            installed: installed(specs),
            profile: profile.to_owned(),
            arch: arch.to_owned(),
        }
    }

    #[test]
    fn item_without_restrictions_is_relevant() {
        let item = NewsItem::parse("x", "Title: Hello\nNews-Item-Format: 1.0\n");
        assert!(item.is_valid());
        assert!(item.is_relevant(&env(&[], "", "amd64")));
    }

    #[test]
    fn invalid_format_is_rejected() {
        let item = NewsItem::parse("x", "Title: T\nNews-Item-Format: 3.0\n");
        assert!(!item.is_valid());
        let no_format = NewsItem::parse("x", "Title: T\n");
        assert!(!no_format.is_valid());
    }

    #[test]
    fn installed_restriction_uses_version_operator() {
        let item = NewsItem::parse(
            "x",
            "Title: T\nNews-Item-Format: 1.0\nDisplay-If-Installed: >=sys-libs/glibc-2.0\n",
        );
        assert!(item.is_relevant(&env(&["sys-libs/glibc-2.5"], "", "amd64")));
        // An older installed version does not satisfy the >= restriction.
        assert!(!item.is_relevant(&env(&["sys-libs/glibc-1.9"], "", "amd64")));
    }

    #[test]
    fn profile_exact_for_format_1_and_prefix_for_format_2() {
        let one = NewsItem::parse(
            "x",
            "Title: T\nNews-Item-Format: 1.0\nDisplay-If-Profile: default/linux/amd64/17.1\n",
        );
        assert!(one.is_relevant(&env(&[], "default/linux/amd64/17.1", "amd64")));
        // Format 1 is exact: a sub-profile does not match.
        assert!(!one.is_relevant(&env(&[], "default/linux/amd64/17.1/desktop", "amd64")));

        let two = NewsItem::parse(
            "x",
            "Title: T\nNews-Item-Format: 2.0\nDisplay-If-Profile: default/linux/amd64/17.1/*\n",
        );
        assert!(two.is_relevant(&env(&[], "default/linux/amd64/17.1/desktop", "amd64")));
        assert!(two.is_relevant(&env(&[], "default/linux/amd64/17.1", "amd64")));
        assert!(!two.is_relevant(&env(&[], "default/linux/x86/17.1", "amd64")));
    }

    #[test]
    fn no_unread_renders_nothing() {
        assert_eq!(render_news(&[]), "");
    }

    #[test]
    fn unread_relevant_reads_with_language_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let news = dir.path().join("metadata/news");
        let item_name = "2024-05-01-glibc";
        let item_dir = news.join(item_name);
        std::fs::create_dir_all(&item_dir).unwrap();
        std::fs::write(
            item_dir.join(format!("{item_name}.en.txt")),
            "Title: glibc upgrade\nNews-Item-Format: 1.0\nDisplay-If-Installed: sys-libs/glibc\n\nBody.\n",
        )
        .unwrap();

        let unread: BTreeSet<String> = [item_name.to_string()].into_iter().collect();
        // A missing `de` body falls back to `en`.
        let relevant = unread_relevant(
            &news,
            &env(&["sys-libs/glibc-2.5"], "", "amd64"),
            &unread,
            "de",
        )
        .unwrap();
        assert_eq!(relevant.len(), 1);
        assert_eq!(relevant[0].title, "glibc upgrade");
    }

    #[test]
    fn missing_news_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let news = dir.path().join("metadata/news");
        let out = unread_relevant(&news, &env(&[], "", "amd64"), &BTreeSet::new(), "en").unwrap();
        assert!(out.is_empty());
    }
}
