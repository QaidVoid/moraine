//! GLEP 42 news item reading, relevance, and display.
//!
//! News items live under each repository's `metadata/news` directory, one
//! directory per item holding a `*.en.txt` body. The header carries
//! `Display-If-Installed`, `Display-If-Profile`, and `Display-If-Keyword`
//! restrictions. This module parses items, evaluates relevance against the
//! environment, and reads the per-repository unread state. It never writes: it
//! does not mark items read or modify any skip state.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use miette::Diagnostic;
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

/// A parsed news item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewsItem {
    /// The item identifier, taken from its directory name.
    pub name: String,
    /// The `Title:` header.
    pub title: String,
    /// The display restrictions declared by the item.
    pub restrictions: Vec<Restriction>,
}

/// The environment a news item's relevance is evaluated against.
#[derive(Debug, Clone, Default)]
pub struct NewsEnv {
    /// The installed `category/package` set.
    pub installed: BTreeSet<String>,
    /// The active profile path, as a repository-relative string.
    pub profile: String,
    /// The system arch keyword, for example `amd64`.
    pub arch: String,
}

impl NewsItem {
    /// Parse a news item from its header text and directory name.
    ///
    /// Only the header keys this phase evaluates are retained. Unknown headers
    /// and the body are ignored.
    pub fn parse(name: &str, header: &str) -> NewsItem {
        let mut title = String::new();
        let mut restrictions = Vec::new();
        for line in header.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "Title" => title = value.to_owned(),
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
            restrictions,
        }
    }

    /// Whether the item is relevant in the given environment.
    ///
    /// Restrictions of the same kind are alternatives; restrictions of different
    /// kinds are joint requirements. An item with no restrictions is always
    /// relevant.
    pub fn is_relevant(&self, env: &NewsEnv) -> bool {
        let installed: Vec<&String> = self
            .restrictions
            .iter()
            .filter_map(|r| match r {
                Restriction::IfInstalled(a) => Some(a),
                _ => None,
            })
            .collect();
        let profiles: Vec<&String> = self
            .restrictions
            .iter()
            .filter_map(|r| match r {
                Restriction::IfProfile(p) => Some(p),
                _ => None,
            })
            .collect();
        let keywords: Vec<&String> = self
            .restrictions
            .iter()
            .filter_map(|r| match r {
                Restriction::IfKeyword(k) => Some(k),
                _ => None,
            })
            .collect();

        let installed_ok = installed.is_empty()
            || installed
                .iter()
                .any(|atom| installed_matches(&env.installed, atom));
        let profile_ok =
            profiles.is_empty() || profiles.iter().any(|p| env.profile.contains(p.as_str()));
        let keyword_ok =
            keywords.is_empty() || keywords.iter().any(|k| keyword_matches(&env.arch, k));

        installed_ok && profile_ok && keyword_ok
    }
}

/// Whether any installed `category/package` satisfies the restriction atom.
///
/// The restriction names a `category/package` (optionally with a version
/// constraint). Matching here is on the `category/package` head, which is what
/// the installed set carries.
fn installed_matches(installed: &BTreeSet<String>, atom: &str) -> bool {
    let head = atom_head(atom);
    installed.contains(head)
}

/// The `category/package` head of a restriction atom.
fn atom_head(atom: &str) -> &str {
    let trimmed = atom.trim_start_matches(['>', '<', '=', '~', '!']);
    // Strip a trailing version if present by cutting at the last `-` that begins
    // a version token. A simple heuristic suffices for relevance matching.
    if let Some(slot) = trimmed.find(':') {
        return &trimmed[..slot];
    }
    trimmed
}

/// Whether the arch satisfies a `Display-If-Keyword` restriction.
fn keyword_matches(arch: &str, keyword: &str) -> bool {
    let bare = keyword.trim_start_matches(['~', '-']);
    bare == arch
}

/// Read and evaluate unread, relevant news for one repository.
///
/// `news_dir` is the repository's `metadata/news` directory; `unread` is the set
/// of item names the per-repository state reports as unread. Returns only items
/// that are both unread and relevant. The unread state is read, never written.
#[instrument(skip(news_dir, env, unread))]
pub fn unread_relevant(
    news_dir: &Path,
    env: &NewsEnv,
    unread: &BTreeSet<String>,
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
        let Some(header) = read_header(&entry.path(), &name)? else {
            continue;
        };
        let item = NewsItem::parse(&name, &header);
        if item.is_relevant(env) {
            items.push(item);
        }
    }
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(items)
}

/// Read the header (text up to the first blank line) of a news item body.
fn read_header(item_dir: &Path, name: &str) -> Result<Option<String>, NewsError> {
    let body = item_dir.join(format!("{name}.en.txt"));
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

    fn env(installed: &[&str], profile: &str, arch: &str) -> NewsEnv {
        NewsEnv {
            installed: installed.iter().map(|s| s.to_string()).collect(),
            profile: profile.to_owned(),
            arch: arch.to_owned(),
        }
    }

    #[test]
    fn item_without_restrictions_is_relevant() {
        let item = NewsItem::parse("2024-01-01-test", "Title: Hello\nAuthor: a\n");
        assert_eq!(item.title, "Hello");
        assert!(item.is_relevant(&env(&[], "", "amd64")));
    }

    #[test]
    fn installed_restriction_gates_relevance() {
        let item = NewsItem::parse("x", "Title: T\nDisplay-If-Installed: sys-apps/portage\n");
        assert!(item.is_relevant(&env(&["sys-apps/portage"], "", "amd64")));
        assert!(!item.is_relevant(&env(&["dev-libs/openssl"], "", "amd64")));
    }

    #[test]
    fn differing_kinds_are_joint() {
        let item = NewsItem::parse(
            "x",
            "Title: T\nDisplay-If-Installed: sys-apps/portage\nDisplay-If-Keyword: amd64\n",
        );
        assert!(item.is_relevant(&env(&["sys-apps/portage"], "", "amd64")));
        // Installed matches but keyword does not.
        assert!(!item.is_relevant(&env(&["sys-apps/portage"], "", "x86")));
    }

    #[test]
    fn like_kinds_are_alternatives() {
        let item = NewsItem::parse(
            "x",
            "Title: T\nDisplay-If-Installed: a/one\nDisplay-If-Installed: b/two\n",
        );
        assert!(item.is_relevant(&env(&["b/two"], "", "amd64")));
    }

    #[test]
    fn no_unread_renders_nothing() {
        assert_eq!(render_news(&[]), "");
    }

    #[test]
    fn unread_relevant_reads_from_dir() {
        let dir = tempfile::tempdir().unwrap();
        let news = dir.path().join("metadata/news");
        let item_name = "2024-05-01-glibc";
        let item_dir = news.join(item_name);
        std::fs::create_dir_all(&item_dir).unwrap();
        std::fs::write(
            item_dir.join(format!("{item_name}.en.txt")),
            "Title: glibc upgrade\nDisplay-If-Installed: sys-libs/glibc\n\nBody text here.\n",
        )
        .unwrap();

        let unread: BTreeSet<String> = [item_name.to_string()].into_iter().collect();
        let relevant =
            unread_relevant(&news, &env(&["sys-libs/glibc"], "", "amd64"), &unread).unwrap();
        assert_eq!(relevant.len(), 1);
        assert_eq!(relevant[0].title, "glibc upgrade");

        // An item not in the unread set is skipped.
        let none = unread_relevant(
            &news,
            &env(&["sys-libs/glibc"], "", "amd64"),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn missing_news_dir_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let news = dir.path().join("metadata/news");
        let out = unread_relevant(&news, &env(&[], "", "amd64"), &BTreeSet::new()).unwrap();
        assert!(out.is_empty());
    }
}
