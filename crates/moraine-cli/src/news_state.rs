//! GLEP 42 news state: per-repository `unread`/`skip` files and display.
//!
//! Each repository's seen and unread items are tracked in
//! `${EROOT}/var/lib/portage/news/news-<repoid>.unread` and `.skip`. The
//! [`update_items`] routine mirrors `lib/portage/news.py`: it lists the repo's
//! `metadata/news`, skips items already seen, validates and evaluates each item,
//! and records newly-seen relevant items in both files. The whole load, mutate,
//! and save cycle is held under an advisory lock on the `.unread` file so
//! concurrent runs do not clobber each other, and the written state files carry
//! the GLEP 42 ownership and permissions. [`display_after_action`] runs this for
//! every repository and prints the unread counts after an install or a sync.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use moraine_repo::RepoSet;

use crate::config::ConfigContext;
use crate::news::{InstalledPkg, NewsEnv, NewsItem};

/// GLEP 42 file mode: group write, world read (`0o0064`).
const GLEP42_FILE_MODE: u32 = 0o0064;

/// A held advisory lock on a news-state lock file: a real `flock(LOCK_EX)` held
/// for the read-modify-write cycle. The lock is released when the handle drops;
/// the lock file is never unlinked, so a concurrent acquirer blocks on the same
/// inode rather than racing a recreated file.
struct NewsLock {
    _file: std::fs::File,
}

impl NewsLock {
    /// Acquire the exclusive lock for `repo_id` under `news_lib`. Returns `None`
    /// when the lock file cannot be created or locked, in which case the caller
    /// proceeds without serialization rather than failing.
    fn acquire(news_lib: &Path, repo_id: &str) -> Option<Self> {
        if let Err(e) = std::fs::create_dir_all(news_lib) {
            tracing::warn!(error = %e, "could not create news state directory");
            return None;
        }
        let path = news_lib.join(format!("news-{repo_id}.unread.lock"));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ok()?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive).ok()?;
        Some(Self { _file: file })
    }
}

/// The per-repository news state: which items have been seen (`skip`) and which
/// remain unread.
#[derive(Debug, Default)]
pub struct NewsState {
    /// Items already evaluated for relevance (the `.skip` file).
    pub skip: BTreeSet<String>,
    /// Items relevant and not yet read (the `.unread` file).
    pub unread: BTreeSet<String>,
}

impl NewsState {
    /// Load the state for `repo_id` under `news_lib`, tolerating absent files.
    pub fn load(news_lib: &Path, repo_id: &str) -> Self {
        NewsState {
            skip: read_lines(&skip_path(news_lib, repo_id)),
            unread: read_lines(&unread_path(news_lib, repo_id)),
        }
    }

    /// Persist both files atomically, then apply the GLEP 42 ownership and
    /// permissions to each. A write failure (for example an unprivileged run) is
    /// logged and ignored, matching `news.py`'s silent `PermissionDenied`.
    pub fn save(&self, news_lib: &Path, repo_id: &str) {
        if let Err(e) = std::fs::create_dir_all(news_lib) {
            tracing::warn!(error = %e, "could not create news state directory");
            return;
        }
        write_lines(&skip_path(news_lib, repo_id), &self.skip);
        write_lines(&unread_path(news_lib, repo_id), &self.unread);
    }
}

/// The `portage` group id, read from `/etc/group` without a libc dependency.
/// `None` when the group is absent or the file cannot be read.
fn portage_gid() -> Option<u32> {
    let text = std::fs::read_to_string("/etc/group").ok()?;
    for line in text.lines() {
        let mut fields = line.split(':');
        if fields.next() == Some("portage") {
            // The gid is the third colon-separated field.
            return fields.nth(1).and_then(|g| g.trim().parse().ok());
        }
    }
    None
}

/// Apply the GLEP 42 attributes to a written state file: owner root, group
/// `portage`, mode `0o0064`. Skipped without privilege: the `0o0064` mode strips
/// the owner's own permissions, so applying it as a non-root user would lock the
/// owner out of its own state. Only the privileged (root) run, which bypasses the
/// owner check, applies the attributes, matching `apply_secpass_permissions`.
fn apply_glep42_attrs(path: &Path) {
    use rustix::fs::{Gid, Mode, Uid};
    if !rustix::process::geteuid().is_root() {
        return;
    }
    if let Some(gid) = portage_gid() {
        let owner = Uid::from_raw(0);
        let group = Gid::from_raw(gid);
        let _ = rustix::fs::chown(path, Some(owner), Some(group));
    }
    let _ = rustix::fs::chmod(path, Mode::from_bits_truncate(GLEP42_FILE_MODE));
}

/// The `${EROOT}/var/lib/portage/news` directory.
pub fn news_lib_path(eroot: &Path) -> PathBuf {
    eroot.join("var/lib/portage/news")
}

fn unread_path(news_lib: &Path, repo_id: &str) -> PathBuf {
    news_lib.join(format!("news-{repo_id}.unread"))
}

fn skip_path(news_lib: &Path, repo_id: &str) -> PathBuf {
    news_lib.join(format!("news-{repo_id}.skip"))
}

/// Update the news state for one repository and return the unread count.
///
/// Every item in the repo's `metadata/news` not already in the skip set is read
/// and validated; relevant items are added to both the unread and skip sets, and
/// invalid or irrelevant items are added to the skip set only so they are not
/// re-evaluated. The state is then persisted.
pub fn update_items(
    news_dir: &Path,
    repo_id: &str,
    env: &NewsEnv,
    news_lib: &Path,
    lang: &str,
) -> usize {
    // Hold the lock across the whole load-mutate-save cycle so a concurrent run
    // cannot read a stale unread set and overwrite our additions.
    let _lock = NewsLock::acquire(news_lib, repo_id);
    let mut state = NewsState::load(news_lib, repo_id);
    if news_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(news_dir)
    {
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            if state.skip.contains(&name) {
                continue;
            }
            // A newly seen item is always skipped from future evaluation; it joins
            // the unread set only when it is valid and relevant.
            state.skip.insert(name.clone());
            if let Some(item) = read_item(news_dir, &name, lang)
                && item.is_valid()
                && item.is_relevant(env)
            {
                state.unread.insert(name);
            }
        }
    }
    state.save(news_lib, repo_id);
    state.unread.len()
}

/// Build the news environment from the loaded configuration and installed store.
pub fn news_env(ctx: &ConfigContext, vdb_dir: &Path) -> NewsEnv {
    NewsEnv {
        installed: installed_packages(vdb_dir),
        profile: repo_relative_profile(ctx),
        arch: ctx.arch.clone(),
    }
}

/// Run [`update_items`] for every configured repository and print the unread
/// counts, mirroring `display_news_notifications`. A repository with no unread
/// items prints nothing.
pub fn display_after_action(ctx: &ConfigContext, vdb_dir: &Path, eroot: &Path, repos: &RepoSet) {
    let env = news_env(ctx, vdb_dir);
    let news_lib = news_lib_path(eroot);
    let lang = news_language(ctx);
    for repo in repos.ordered() {
        let news_dir = repo.location.join("metadata/news");
        let count = update_items(&news_dir, &repo.name, &env, &news_lib, &lang);
        if count > 0 {
            println!(
                "\n * IMPORTANT: {count} news item{} need reading for repository '{}'.",
                if count == 1 { "" } else { "s" },
                repo.name
            );
        }
    }
}

/// The configured news body language, defaulting to `en`.
fn news_language(ctx: &ConfigContext) -> String {
    ctx.vars
        .get("PORTAGE_NEWS_LANG")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("en")
        .to_owned()
}

/// Read and parse one news item header, with the language fallback.
fn read_item(news_dir: &Path, name: &str, lang: &str) -> Option<NewsItem> {
    let item_dir = news_dir.join(name);
    let mut body = item_dir.join(format!("{name}.{lang}.txt"));
    if !body.exists() {
        body = item_dir.join(format!("{name}.en.txt"));
    }
    let text = std::fs::read_to_string(&body).ok()?;
    let header = text.split("\n\n").next().unwrap_or("");
    Some(NewsItem::parse(name, header))
}

/// Read the installed packages for full-atom `Display-If-Installed` matching.
fn installed_packages(vdb_dir: &Path) -> Vec<InstalledPkg> {
    let Ok(store) = crate::write::load_installed_store(vdb_dir) else {
        return Vec::new();
    };
    let interner = store.interner();
    store
        .records()
        .iter()
        .map(|r| InstalledPkg {
            category: interner
                .resolve(r.category)
                .map(|s| s.to_string())
                .unwrap_or_default(),
            package: interner
                .resolve(r.package)
                .map(|s| s.to_string())
                .unwrap_or_default(),
            version: r.version.clone(),
            slot: interner
                .resolve(r.slot.slot)
                .map(|s| s.to_string())
                .unwrap_or_default(),
            subslot: r
                .slot
                .subslot
                .and_then(|s| interner.resolve(s))
                .map(|s| s.to_string()),
        })
        .collect()
}

/// The active profile path relative to its owning repository's `profiles/`
/// directory, the form `Display-If-Profile` is compared against.
fn repo_relative_profile(ctx: &ConfigContext) -> String {
    let Some(node) = ctx.profile.nodes.last() else {
        return String::new();
    };
    let path = std::fs::canonicalize(&node.path).unwrap_or_else(|_| node.path.clone());
    let needle = format!(
        "{}profiles{}",
        std::path::MAIN_SEPARATOR,
        std::path::MAIN_SEPARATOR
    );
    let s = path.to_string_lossy();
    match s.find(&needle) {
        Some(idx) => s[idx + needle.len()..].to_owned(),
        None => s.into_owned(),
    }
}

fn read_lines(path: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn write_lines(path: &Path, lines: &BTreeSet<String>) {
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    match moraine_common::fs::atomic_write(path, body.as_bytes()) {
        Ok(()) => apply_glep42_attrs(path),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "could not write news state")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moraine_version::Version;

    fn write_item(news_dir: &Path, name: &str, body: &str) {
        let dir = news_dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.en.txt")), body).unwrap();
    }

    fn env(installed: &[InstalledPkg]) -> NewsEnv {
        NewsEnv {
            installed: installed.to_vec(),
            profile: String::new(),
            arch: "amd64".to_owned(),
        }
    }

    #[test]
    fn update_items_seeds_unread_and_skip_then_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let news_dir = tmp.path().join("metadata/news");
        write_item(
            &news_dir,
            "2024-01-01-a",
            "Title: A\nNews-Item-Format: 1.0\nDisplay-If-Installed: sys-libs/glibc\n\nBody.\n",
        );
        write_item(
            &news_dir,
            "2024-01-02-b",
            "Title: B\nNews-Item-Format: 1.0\nDisplay-If-Installed: dev-libs/absent\n\nBody.\n",
        );
        let news_lib = tmp.path().join("newslib");
        let installed = vec![InstalledPkg {
            category: "sys-libs".to_owned(),
            package: "glibc".to_owned(),
            version: Version::parse("2.5").unwrap(),
            slot: "0".to_owned(),
            subslot: None,
        }];

        let count = update_items(&news_dir, "gentoo", &env(&installed), &news_lib, "en");
        assert_eq!(count, 1, "only the relevant item is unread");

        let state = NewsState::load(&news_lib, "gentoo");
        // Both items were seen, only the relevant one is unread.
        assert_eq!(state.skip.len(), 2);
        assert!(state.unread.contains("2024-01-01-a"));
        assert!(!state.unread.contains("2024-01-02-b"));

        // A second run sees no new items and keeps the same unread count.
        let again = update_items(&news_dir, "gentoo", &env(&installed), &news_lib, "en");
        assert_eq!(again, 1);
    }

    #[test]
    fn concurrent_updates_do_not_lose_unread_items() {
        let tmp = tempfile::tempdir().unwrap();
        let news_dir = tmp.path().join("metadata/news");
        // Many relevant items, all seeded fresh on the first scan to see.
        for i in 0..40 {
            write_item(
                &news_dir,
                &format!("2024-02-{i:02}"),
                "Title: T\nNews-Item-Format: 1.0\nDisplay-If-Installed: sys-libs/glibc\n\nBody.\n",
            );
        }
        let news_lib = tmp.path().join("newslib");
        let installed = vec![InstalledPkg {
            category: "sys-libs".to_owned(),
            package: "glibc".to_owned(),
            version: Version::parse("2.5").unwrap(),
            slot: "0".to_owned(),
            subslot: None,
        }];

        // Two threads scan the same repository concurrently. The lockfile must
        // serialize the read-modify-write so no unread item is dropped.
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let news_dir = news_dir.clone();
                let news_lib = news_lib.clone();
                let env = env(&installed);
                scope.spawn(move || {
                    update_items(&news_dir, "gentoo", &env, &news_lib, "en");
                });
            }
        });

        let state = NewsState::load(&news_lib, "gentoo");
        assert_eq!(state.unread.len(), 40);
        assert_eq!(state.skip.len(), 40);
    }
}
