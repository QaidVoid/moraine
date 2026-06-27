//! The Moraine merge engine: the only crate that mutates the live filesystem.
//!
//! `moraine-merge` is the final write-path phase. It takes a built image from
//! `moraine-build`, the installed store from `moraine-vdb`, and the ordered task
//! list from `moraine-resolve`, and applies each merge or unmerge to the live
//! root (`EROOT`) atomically. It computes CONTENTS records, enforces collision
//! protection, honors `CONFIG_PROTECT`, preserves still-needed shared libraries,
//! unmerges safely, records the installed state, and updates `@world`.
//!
//! # Crash safety
//!
//! A merge is a transition that is durable before the package becomes visible in
//! the installed store. Each operation writes an in-progress marker before any
//! mutation and clears it at the commit point. On invocation the engine scans for
//! markers and recovers an interrupted operation deterministically. See
//! [`recovery`].
//!
//! # The single write surface
//!
//! Every other crate treats the installed store as read-only. The engine holds a
//! process-wide installed-store lock for the duration of an operation and applies
//! operations strictly in task-list order, one at a time. See [`MergeEngine`].
//!
//! # Inputs as data
//!
//! The engine takes the policy it needs (install root, FEATURES, CONFIG_PROTECT)
//! as plain input structs rather than reaching into `moraine-config` accessors,
//! so it is self-contained and the dangerous write surface is easy to drive in
//! tests against a tempdir root.

pub mod collision;
pub mod contents;
pub mod error;
pub mod image;
pub mod install_mask;
pub mod plan;
pub mod preserve;
pub mod protect;
pub mod recovery;
pub mod state;

mod engine;
mod merge;
mod unmerge;

pub use collision::{Collision, CollisionKind};
pub use contents::compute_md5;
pub use engine::{MergeEngine, OperationOutcome};
pub use error::MergeError;
pub use plan::{MergeOp, Operation, UnmergeOp};
pub use preserve::{PreservedEntry, PreservedLibs};
pub use protect::{ConfigProtect, variant_name};
pub use state::{ElogRecord, PackageState, PostMergeReport, rewrite_slot_operators};

use std::path::{Path, PathBuf};

/// The FEATURES tokens the merge engine honors, parsed from a FEATURES list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Features {
    /// `collision-protect`: any collision aborts the merge before mutation.
    pub collision_protect: bool,
    /// `protect-owned`: a collision with a file owned by another package aborts.
    pub protect_owned: bool,
    /// `preserve-libs`: still-needed shared libraries are preserved on removal.
    pub preserve_libs: bool,
    /// `xattr`: extended attributes are captured from the image and reapplied on
    /// placement.
    pub xattr: bool,
    /// `sandbox`: the build/merge sandbox is requested (carried, not enforced
    /// here, so it is not silently dropped).
    pub sandbox: bool,
    /// `usersandbox`: the per-user sandbox variant is requested.
    pub usersandbox: bool,
    /// `config-protect-if-modified`: a protected file is only shielded when the
    /// admin has modified it from what was last installed; an unmodified file is
    /// overwritten in place. Default on, matching Portage.
    pub config_protect_if_modified: bool,
    /// `unmerge-orphans`: on unmerge, unlink a non-directory, non-symlink-to-dir,
    /// non-protected, non-preserved file still owned by the package regardless of
    /// md5 or mtime drift.
    pub unmerge_orphans: bool,
}

impl Features {
    /// Parse the FEATURES tokens, recognizing the merge-relevant flags.
    pub fn from_tokens<'a>(tokens: impl IntoIterator<Item = &'a str>) -> Self {
        let mut f = Features::default();
        for token in tokens {
            match token {
                "collision-protect" => f.collision_protect = true,
                "protect-owned" => f.protect_owned = true,
                "preserve-libs" => f.preserve_libs = true,
                "xattr" => f.xattr = true,
                "sandbox" => f.sandbox = true,
                "usersandbox" => f.usersandbox = true,
                "config-protect-if-modified" => f.config_protect_if_modified = true,
                "unmerge-orphans" => f.unmerge_orphans = true,
                _ => {}
            }
        }
        f
    }
}

/// The live-system context an operation runs against.
///
/// `eroot` is the install root, normally `/`, set to a tempdir in tests. The
/// installed store lives under `vdb_dir`, and the lock plus markers and the
/// preserved-libs registry live under `state_dir`.
#[derive(Debug, Clone)]
pub struct MergeContext {
    /// The install root (EROOT) that files are merged into.
    pub eroot: PathBuf,
    /// The directory holding the installed store files.
    pub vdb_dir: PathBuf,
    /// The directory holding the lock, in-progress markers, the world file, the
    /// counter, and the preserved-libs registry.
    pub state_dir: PathBuf,
    /// The enabled FEATURES relevant to merging.
    pub features: Features,
    /// The CONFIG_PROTECT policy.
    pub config_protect: ConfigProtect,
    /// `COLLISION_IGNORE` globs: target paths matching any are exempt from
    /// collision detection.
    pub collision_ignore: Vec<String>,
    /// `UNINSTALL_IGNORE` globs: owned paths matching any are never removed on
    /// unmerge.
    pub uninstall_ignore: Vec<String>,
    /// The combined `INSTALL_MASK`/`PKG_INSTALL_MASK` filter: matching image
    /// paths are dropped before they enter CONTENTS.
    pub install_mask: install_mask::InstallMask,
}

impl MergeContext {
    /// Map an install-root-relative absolute path to its live filesystem path
    /// under [`eroot`](Self::eroot).
    pub(crate) fn live_path(&self, install_path: &str) -> PathBuf {
        let rel = install_path.trim_start_matches('/');
        self.eroot.join(rel)
    }

    /// The world file path under the state directory.
    pub(crate) fn world_file(&self) -> PathBuf {
        self.state_dir.join("world")
    }

    /// The global counter file path under the state directory.
    pub(crate) fn counter_file(&self) -> PathBuf {
        self.state_dir.join("counter")
    }

    /// The preserved-libs registry path under the state directory.
    pub(crate) fn registry_file(&self) -> PathBuf {
        self.state_dir.join("preserved-libs")
    }

    /// The lock file path under the state directory.
    pub(crate) fn lock_file(&self) -> PathBuf {
        self.state_dir.join("vdb.lock")
    }

    /// The config-memory file (`CONFIG_MEMORY_FILE`) under the state directory,
    /// recording the config-file contents already offered to the admin.
    pub(crate) fn confmem_file(&self) -> PathBuf {
        self.state_dir.join("config-memory")
    }

    /// The in-progress marker directory under the state directory.
    pub(crate) fn marker_dir(&self) -> PathBuf {
        self.state_dir.join("in-progress")
    }
}

/// Whether `path` is matched by any of the `fnmatch`-style `globs`, used for
/// `COLLISION_IGNORE` and `UNINSTALL_IGNORE`. A glob matches when `path` equals
/// it, when `path` falls under it as a directory prefix, or when the `*`/`?`
/// pattern matches, with `*` spanning path separators as Portage's `fnmatch`
/// does.
pub(crate) fn path_matches_any(path: &str, globs: &[String]) -> bool {
    globs.iter().any(|g| {
        let g = g.trim_end_matches('/');
        path == g || path.starts_with(&format!("{g}/")) || fnmatch(path, g)
    })
}

/// A minimal `fnmatch` supporting `*` (any run, separators included), `?` (any
/// single character), and `[seq]`/`[!seq]` character classes including `a-z`
/// ranges, matching the patterns Python's `fnmatch.translate` matches. An
/// unterminated `[` is treated as a literal `[`, as Python does.
pub(crate) fn fnmatch(text: &str, pat: &str) -> bool {
    let (t, p) = (text.as_bytes(), pat.as_bytes());
    // Iterative backtracking match.
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star_p, mut star_t): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star_p = Some(pi);
            star_t = ti;
            pi += 1;
            continue;
        }
        // The pattern position past a single matched unit, or `None` on mismatch.
        let next = if pi < p.len() {
            match p[pi] {
                b'?' => Some(pi + 1),
                b'[' => match match_class(p, pi, t[ti]) {
                    Some((true, end)) => Some(end),
                    Some((false, _)) => None,
                    // An unterminated `[` is a literal `[`.
                    None => (p[pi] == t[ti]).then_some(pi + 1),
                },
                c => (c == t[ti]).then_some(pi + 1),
            }
        } else {
            None
        };
        match next {
            Some(np) => {
                pi = np;
                ti += 1;
            }
            None => match star_p {
                Some(sp) => {
                    pi = sp + 1;
                    star_t += 1;
                    ti = star_t;
                }
                None => return false,
            },
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// Match a `[...]` character class in `pat` at `pi` (the opening `[`) against
/// `ch`. Returns whether `ch` matches and the index just past the closing `]`, or
/// `None` when the class is unterminated so the caller treats `[` as a literal. A
/// leading `!` or `^` negates; a `]` immediately after the opening (or negation)
/// is a literal member; `a-z` denotes an inclusive range.
fn match_class(pat: &[u8], pi: usize, ch: u8) -> Option<(bool, usize)> {
    let mut j = pi + 1;
    let negate = matches!(pat.get(j), Some(b'!') | Some(b'^'));
    if negate {
        j += 1;
    }
    let start = j;
    let mut matched = false;
    loop {
        match pat.get(j) {
            // Unterminated class.
            None => return None,
            // A closing `]` after at least one member ends the class; a `]` in the
            // first position is a literal member instead.
            Some(b']') if j > start => break,
            Some(&c) => {
                if pat.get(j + 1) == Some(&b'-') && pat.get(j + 2).is_some_and(|&e| e != b']') {
                    let end = pat[j + 2];
                    if c <= ch && ch <= end {
                        matched = true;
                    }
                    j += 3;
                } else {
                    if c == ch {
                        matched = true;
                    }
                    j += 1;
                }
            }
        }
    }
    Some((matched ^ negate, j + 1))
}

/// Read the directory entry names directly under `dir`, returning an empty list
/// when the directory does not exist.
pub(crate) fn dir_entry_names(dir: &Path) -> Vec<String> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    read.filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_parse_tokens() {
        let f = Features::from_tokens([
            "sandbox",
            "usersandbox",
            "preserve-libs",
            "collision-protect",
            "xattr",
            "config-protect-if-modified",
            "unmerge-orphans",
        ]);
        assert!(f.preserve_libs);
        assert!(f.collision_protect);
        assert!(!f.protect_owned);
        // sandbox/usersandbox are carried rather than silently dropped.
        assert!(f.sandbox);
        assert!(f.usersandbox);
        assert!(f.xattr);
        assert!(f.config_protect_if_modified);
        assert!(f.unmerge_orphans);
    }

    #[test]
    fn fnmatch_character_classes() {
        // Positive class.
        assert!(fnmatch("a", "[abc]"));
        assert!(!fnmatch("d", "[abc]"));
        // Negated class (both spellings).
        assert!(fnmatch("d", "[!abc]"));
        assert!(!fnmatch("a", "[!abc]"));
        assert!(fnmatch("d", "[^abc]"));
        // Range.
        assert!(fnmatch("5", "[0-9]"));
        assert!(!fnmatch("x", "[0-9]"));
        // A leading `]` is a literal member.
        assert!(fnmatch("]", "[]a]"));
        // Class embedded in a larger pattern.
        assert!(fnmatch("/usr/lib/libfoo.so.1", "/usr/lib/*.so.[0-9]*"));
        assert!(!fnmatch("/usr/lib/libfoo.so.x", "/usr/lib/*.so.[0-9]*"));
        // An unterminated `[` is a literal `[`.
        assert!(fnmatch("a[b", "a[b"));
        assert!(!fnmatch("ab", "a[b"));
    }

    #[test]
    fn install_mask_inherits_character_classes() {
        let mask = install_mask::InstallMask::new("/usr/lib/*.so.[0-9]*");
        assert!(mask.is_masked("/usr/lib/libfoo.so.1"));
        assert!(!mask.is_masked("/usr/lib/libfoo.so"));
    }

    #[test]
    fn path_matches_any_inherits_character_classes() {
        let globs = vec!["/var/log/app[0-9].log".to_string()];
        assert!(path_matches_any("/var/log/app7.log", &globs));
        assert!(!path_matches_any("/var/log/appx.log", &globs));
    }

    #[test]
    fn path_glob_matching() {
        let globs = vec!["/etc/*.conf".to_string(), "/opt/app".to_string()];
        assert!(path_matches_any("/etc/foo.conf", &globs));
        assert!(path_matches_any("/etc/a/b.conf", &globs));
        assert!(path_matches_any("/opt/app", &globs));
        assert!(path_matches_any("/opt/app/bin/x", &globs));
        assert!(!path_matches_any("/etc/foo.cfg", &globs));
        assert!(!path_matches_any("/usr/bin/x", &globs));
    }

    #[test]
    fn live_path_joins_under_eroot() {
        let ctx = MergeContext {
            eroot: PathBuf::from("/tmp/root"),
            vdb_dir: PathBuf::from("/tmp/vdb"),
            state_dir: PathBuf::from("/tmp/state"),
            features: Features::default(),
            config_protect: ConfigProtect::default(),
            collision_ignore: Vec::new(),
            uninstall_ignore: Vec::new(),
            install_mask: Default::default(),
        };
        assert_eq!(
            ctx.live_path("/usr/bin/foo"),
            PathBuf::from("/tmp/root/usr/bin/foo")
        );
    }
}
