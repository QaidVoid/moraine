//! `profiles/updates/` directive parsing and reading.
//!
//! Gentoo ships package renames and slot changes as `move`/`slotmove` directives
//! under each repository's `profiles/updates/<quarter>` files. This module parses
//! those directives into a typed [`UpdateCommand`] and reads the update files in
//! cumulative mtime order, mirroring Portage's `grab_updates`/`parse_updates`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use moraine_atom::{Atom, Blocker};
use moraine_common::{Interner, Symbol};
use moraine_eapi::features_for_level;

use crate::error::Result;

/// A bare `category/package` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cp {
    /// The interned category.
    pub category: Symbol,
    /// The interned package name.
    pub package: Symbol,
}

/// A parsed `profiles/updates/` directive.
#[derive(Debug, Clone)]
pub enum UpdateCommand {
    /// `move <old-cp> <new-cp>`: rename a package.
    Move {
        /// The old `category/package`.
        old: Cp,
        /// The new `category/package`.
        new: Cp,
    },
    /// `slotmove <atom> <old-slot> <new-slot>`: re-slot matching packages.
    SlotMove {
        /// The package atom the move applies to.
        atom: Atom,
        /// The interned old slot.
        old_slot: Symbol,
        /// The interned new slot.
        new_slot: Symbol,
    },
}

/// A malformed update line and why it was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateError {
    /// The verbatim line.
    pub line: String,
    /// A short reason.
    pub reason: String,
}

/// Parse the text of one update file, returning the valid directives and the
/// malformed lines (which are skipped, never aborting the file).
pub fn parse_updates(text: &str, interner: &Interner) -> (Vec<UpdateCommand>, Vec<UpdateError>) {
    let mut cmds = Vec::new();
    let mut errors = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match parse_line(trimmed, interner) {
            Ok(cmd) => cmds.push(cmd),
            Err(reason) => errors.push(UpdateError {
                line: trimmed.to_string(),
                reason,
            }),
        }
    }
    (cmds, errors)
}

/// Parse a single non-blank, non-comment update line.
fn parse_line(line: &str, interner: &Interner) -> std::result::Result<UpdateCommand, String> {
    // The latest feature set so slot/use atoms parse; updates use modern syntax.
    let features = features_for_level(8);
    let tokens: Vec<&str> = line.split_whitespace().collect();
    match tokens.first().copied() {
        Some("move") => {
            if tokens.len() != 3 {
                return Err("move requires exactly two operands".to_string());
            }
            let old = parse_cp(tokens[1], features, interner)?;
            let new = parse_cp(tokens[2], features, interner)?;
            Ok(UpdateCommand::Move { old, new })
        }
        Some("slotmove") => {
            if tokens.len() != 4 {
                return Err("slotmove requires an atom and two slots".to_string());
            }
            let atom = Atom::parse(tokens[1], features, interner)
                .map_err(|e| format!("invalid slotmove atom: {}", e.reason))?;
            if atom.blocker() != Blocker::None {
                return Err("slotmove atom must not be a blocker".to_string());
            }
            let old_slot = parse_slot(tokens[2], interner)?;
            let new_slot = parse_slot(tokens[3], interner)?;
            Ok(UpdateCommand::SlotMove {
                atom,
                old_slot,
                new_slot,
            })
        }
        Some(other) => Err(format!("unknown update command `{other}`")),
        None => Err("empty update line".to_string()),
    }
}

/// Parse a bare `category/package` operand: a non-blocker, unversioned, slotless,
/// USE-less, repo-less atom.
fn parse_cp(
    token: &str,
    features: moraine_eapi::EapiFeatures,
    interner: &Interner,
) -> std::result::Result<Cp, String> {
    let atom = Atom::parse(token, features, interner)
        .map_err(|e| format!("invalid cp `{token}`: {}", e.reason))?;
    if atom.blocker() != Blocker::None {
        return Err(format!("move operand `{token}` must not be a blocker"));
    }
    if atom.version().is_some() {
        return Err(format!("move operand `{token}` must not be versioned"));
    }
    // A bare `cat/pkg-1` parses with no version operator but its package name ends
    // in a version; reject it as Portage's `catpkgsplit` does.
    if let Some(pkg) = interner.resolve(atom.package())
        && package_has_version_suffix(&pkg)
    {
        return Err(format!("move operand `{token}` must not be versioned"));
    }
    if atom.slot().is_some() || atom.slot_op().is_some() {
        return Err(format!("move operand `{token}` must not carry a slot"));
    }
    if !atom.use_deps().is_empty() || atom.repo().is_some() {
        return Err(format!("move operand `{token}` must be a bare cp"));
    }
    Ok(Cp {
        category: atom.category(),
        package: atom.package(),
    })
}

/// Whether a package name ends in a hyphen-delimited version (so `cat/pkg-1` is
/// not a bare cp), mirroring Portage's `catpkgsplit` version detection.
fn package_has_version_suffix(package: &str) -> bool {
    match package.rsplit_once('-') {
        Some((base, tail)) => {
            !base.is_empty()
                && tail.starts_with(|c: char| c.is_ascii_digit())
                && moraine_version::Version::parse(tail).is_ok()
        }
        None => false,
    }
}

/// Validate and intern a slot token: non-empty, no `/` (the EAPI-5 sub-slot form
/// is rejected for slotmove, matching Portage).
fn parse_slot(token: &str, interner: &Interner) -> std::result::Result<Symbol, String> {
    if token.is_empty() {
        return Err("empty slot token".to_string());
    }
    if token.contains('/') {
        return Err(format!("slot token `{token}` must not contain a sub-slot"));
    }
    if !token
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '.' | '-'))
    {
        return Err(format!("slot token `{token}` has invalid characters"));
    }
    Ok(interner.intern(token))
}

/// One update file to apply: its path and current mtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFile {
    /// The update file path.
    pub path: PathBuf,
    /// Its current mtime (seconds).
    pub mtime: i64,
}

/// List `<repo>/profiles/updates/`, sorted, skipping dotfiles and non-regular
/// files, and return the first file whose mtime differs from `last_mtimes` plus
/// every file after it (cumulative ordering), each with its current mtime. An
/// absent updates directory yields an empty list.
pub fn grab_updates(repo: &Path, last_mtimes: &BTreeMap<PathBuf, i64>) -> Result<Vec<UpdateFile>> {
    let dir = repo.join("profiles/updates");
    let read = match std::fs::read_dir(&dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(moraine_common::CommonError::Io { path: dir, source }.into());
        }
    };

    let mut files: Vec<UpdateFile> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        files.push(UpdateFile {
            path,
            mtime: mtime_of(&meta),
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));

    // From the first mtime-changed file onward, everything is returned.
    let first_changed = files
        .iter()
        .position(|f| last_mtimes.get(&f.path).copied() != Some(f.mtime));
    match first_changed {
        Some(idx) => Ok(files.split_off(idx)),
        None => Ok(Vec::new()),
    }
}

/// The mtime (whole seconds) of a metadata, zero on a pre-epoch clock.
fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Load the persisted update-file mtime map, returning an empty map when absent.
/// Each line is `<path>\t<mtime>`.
pub fn load_mtimes(path: &Path) -> BTreeMap<PathBuf, i64> {
    let mut map = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines() {
            if let Some((p, m)) = line.rsplit_once('\t')
                && let Ok(mtime) = m.trim().parse::<i64>()
            {
                map.insert(PathBuf::from(p), mtime);
            }
        }
    }
    map
}

/// Persist the update-file mtime map atomically.
pub fn store_mtimes(path: &Path, map: &BTreeMap<PathBuf, i64>) -> Result<()> {
    let mut out = String::new();
    for (p, m) in map {
        out.push_str(&p.to_string_lossy());
        out.push('\t');
        out.push_str(&m.to_string());
        out.push('\n');
    }
    moraine_common::fs::atomic_write(path, out.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_move_and_slotmove() {
        let i = Interner::new();
        let (cmds, errs) = parse_updates(
            "move dev-util/foo dev-libs/foo\nslotmove dev-libs/bar 0 1\n",
            &i,
        );
        assert!(errs.is_empty());
        assert_eq!(cmds.len(), 2);
        match &cmds[0] {
            UpdateCommand::Move { old, new } => {
                assert_eq!(i.resolve(old.category).as_deref(), Some("dev-util"));
                assert_eq!(i.resolve(new.category).as_deref(), Some("dev-libs"));
                assert_eq!(i.resolve(new.package).as_deref(), Some("foo"));
            }
            _ => panic!("expected move"),
        }
        match &cmds[1] {
            UpdateCommand::SlotMove {
                old_slot, new_slot, ..
            } => {
                assert_eq!(i.resolve(*old_slot).as_deref(), Some("0"));
                assert_eq!(i.resolve(*new_slot).as_deref(), Some("1"));
            }
            _ => panic!("expected slotmove"),
        }
    }

    #[test]
    fn rejects_malformed_lines_but_keeps_valid_ones() {
        let i = Interner::new();
        let text = "\
# a comment
move dev-util/foo dev-libs/foo
move dev-util/foo
move dev-util/foo dev-libs/foo extra
bogus dev-util/foo dev-libs/foo
move !dev-util/foo dev-libs/foo
move dev-util/foo-1 dev-libs/foo
slotmove dev-libs/bar 0 2/3
";
        let (cmds, errs) = parse_updates(text, &i);
        assert_eq!(cmds.len(), 1, "only the one valid move survives");
        assert_eq!(errs.len(), 6, "every malformed line is reported");
    }

    #[test]
    fn rejects_blocker_and_versioned_and_subslot() {
        let i = Interner::new();
        assert!(parse_line("move !!dev-util/foo dev-libs/foo", &i).is_err());
        assert!(parse_line("move dev-util/foo dev-libs/foo:0", &i).is_err());
        assert!(parse_line("slotmove !dev-libs/bar 0 1", &i).is_err());
        // EAPI-5 sub-slot form in a slot token is rejected.
        assert!(parse_line("slotmove dev-libs/bar 0 1/2", &i).is_err());
    }

    #[test]
    fn grab_updates_returns_cumulative_from_first_change() {
        let dir = tempfile::tempdir().unwrap();
        let updates = dir.path().join("profiles/updates");
        std::fs::create_dir_all(&updates).unwrap();
        for name in ["1Q-2023", "2Q-2023", "3Q-2023", ".hidden"] {
            std::fs::write(updates.join(name), "move a/b a/c\n").unwrap();
        }

        // First run: no recorded mtimes, so all three (sorted, dotfile skipped).
        let empty = BTreeMap::new();
        let first = grab_updates(dir.path(), &empty).unwrap();
        assert_eq!(first.len(), 3);
        assert!(first.iter().all(|f| !f.path.ends_with(".hidden")));

        // Record all current mtimes; a second run returns nothing.
        let recorded: BTreeMap<_, _> = first.iter().map(|f| (f.path.clone(), f.mtime)).collect();
        assert!(grab_updates(dir.path(), &recorded).unwrap().is_empty());

        // Change the middle file's recorded mtime: it and every later file return.
        let mut stale = recorded.clone();
        let middle = updates.join("2Q-2023");
        stale.insert(middle.clone(), 0);
        let changed = grab_updates(dir.path(), &stale).unwrap();
        assert_eq!(changed.len(), 2);
        assert_eq!(changed[0].path, middle);
    }
}
