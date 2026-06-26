//! Global package-move replay (the post-sync `_global_updates` pass).
//!
//! After a changed sync, this reads each synced repository's `profiles/updates/`
//! directives and replays them across the installed store, the world favorites
//! file, `/etc/portage/package.*`, and the binhost index, so a renamed or
//! re-slotted package is referred to by its current name everywhere.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use moraine_atom::{rewrite_dep_cp, rewrite_dep_slot};
use moraine_common::Interner;
use moraine_eapi::features_for_level;
use moraine_repo::{UpdateCommand, grab_updates, parse_updates};
use moraine_vdb::store::Store;

use crate::error::{InstallError, Result};

/// What a global-update pass changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobalUpdateReport {
    /// Installed records renamed by a `move`.
    pub vdb_renames: usize,
    /// Installed records re-slotted by a `slotmove`.
    pub vdb_slotmoves: usize,
    /// Installed records whose dependency atoms were rewritten.
    pub dep_rewrites: usize,
    /// World favorites entries renamed.
    pub world_renames: usize,
    /// `/etc/portage` config files changed.
    pub config_files_changed: usize,
    /// Update files whose new mtimes should be recorded after a successful pass.
    pub applied_files: Vec<(PathBuf, i64)>,
}

/// A move/slotmove resolved to strings, tagged with the repository it came from.
struct Resolved {
    repo: String,
    moves: Vec<(String, String)>,
    slotmoves: Vec<(String, String, String)>,
}

/// Replay the package moves of the synced `repos` (each `(name, path)`) across
/// the installed `store`, the `world_path` file, and the `config_dir`
/// (`/etc/portage`). `mtimes` gates which update files are applied; the caller
/// persists the returned `applied_files` mtimes only after the whole pass
/// commits.
pub fn global_update(
    store: &mut Store,
    repos: &[(String, PathBuf)],
    world_path: &Path,
    config_dir: &Path,
    mtimes: &BTreeMap<PathBuf, i64>,
) -> Result<GlobalUpdateReport> {
    let mut report = GlobalUpdateReport::default();
    let interner = store.interner().clone();

    let mut resolved: Vec<Resolved> = Vec::new();
    for (name, path) in repos {
        let files = grab_updates(path, mtimes).map_err(|e| InstallError::Realize {
            cpv: format!("updates for {name}"),
            reason: e.to_string(),
        })?;
        let mut moves = Vec::new();
        let mut slotmoves = Vec::new();
        for file in files {
            let text =
                std::fs::read_to_string(&file.path).map_err(|e| InstallError::io(&file.path, e))?;
            let (cmds, _errors) = parse_updates(&text, &interner);
            for cmd in cmds {
                push_command(&interner, cmd, &mut moves, &mut slotmoves);
            }
            report.applied_files.push((file.path, file.mtime));
        }
        if !moves.is_empty() || !slotmoves.is_empty() {
            resolved.push(Resolved {
                repo: name.clone(),
                moves,
                slotmoves,
            });
        }
    }

    // Order per Portage: world, then VDB/binpkg, then config files, so the world
    // repo-match still sees the pre-move installed set.
    for r in &resolved {
        for (old, new) in &r.moves {
            if world_repo_match(store, &interner, old, new, &r.repo) {
                report.world_renames += rename_world(world_path, old, new, &interner)? as usize;
            }
        }
    }

    let mut all_moves: Vec<(String, String)> = Vec::new();
    let mut all_slotmoves: Vec<(String, String, String)> = Vec::new();
    for r in &resolved {
        for (old, new) in &r.moves {
            report.vdb_renames += store.move_ent(old, new, Some(&r.repo)).map_err(vdb_err)?;
            all_moves.push((old.clone(), new.clone()));
        }
        for (cp, os, ns) in &r.slotmoves {
            report.vdb_slotmoves += store
                .move_slot_ent(cp, os, ns, Some(&r.repo))
                .map_err(vdb_err)?;
            all_slotmoves.push((cp.clone(), os.clone(), ns.clone()));
        }
    }
    report.dep_rewrites += store
        .update_ents(&all_moves, &all_slotmoves)
        .map_err(vdb_err)?;

    report.config_files_changed +=
        update_config_files(config_dir, &all_moves, &all_slotmoves, &interner)?;

    Ok(report)
}

/// Resolve a parsed directive's symbols to strings.
fn push_command(
    interner: &Interner,
    cmd: UpdateCommand,
    moves: &mut Vec<(String, String)>,
    slotmoves: &mut Vec<(String, String, String)>,
) {
    let cp = |c: moraine_repo::Cp| {
        format!(
            "{}/{}",
            interner.resolve(c.category).unwrap_or_default(),
            interner.resolve(c.package).unwrap_or_default()
        )
    };
    match cmd {
        UpdateCommand::Move { old, new } => moves.push((cp(old), cp(new))),
        UpdateCommand::SlotMove {
            atom,
            old_slot,
            new_slot,
        } => {
            let atom_cp = format!(
                "{}/{}",
                interner.resolve(atom.category()).unwrap_or_default(),
                interner.resolve(atom.package()).unwrap_or_default()
            );
            slotmoves.push((
                atom_cp,
                interner.resolve(old_slot).unwrap_or_default().to_string(),
                interner.resolve(new_slot).unwrap_or_default().to_string(),
            ));
        }
    }
}

/// Whether an installed record of the old or new cp comes from `repo`, the
/// condition Portage uses before rewriting a world entry.
fn world_repo_match(store: &Store, interner: &Interner, old: &str, new: &str, repo: &str) -> bool {
    store.records().iter().any(|r| {
        let cp = format!(
            "{}/{}",
            interner.resolve(r.category).unwrap_or_default(),
            interner.resolve(r.package).unwrap_or_default()
        );
        (cp == old || cp == new)
            && r.repository.and_then(|s| interner.resolve(s)).as_deref() == Some(repo)
    })
}

/// Rewrite the world favorites file, swapping the old cp for the new in every
/// entry, keeping it sorted and unique. Returns whether anything changed.
fn rename_world(path: &Path, old: &str, new: &str, interner: &Interner) -> Result<bool> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(InstallError::io(path, e)),
    };
    let features = features_for_level(8);
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut changed = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let rewritten = rewrite_dep_cp(trimmed, old, new, "", features, interner);
        if rewritten != trimmed {
            changed = true;
        }
        set.insert(rewritten);
    }
    if changed {
        let mut out = String::new();
        for line in &set {
            out.push_str(line);
            out.push('\n');
        }
        write_atomic(path, out.as_bytes())?;
    }
    Ok(changed)
}

/// The `/etc/portage` files (and directory forms) whose atoms are rewritten.
const CONFIG_TARGETS: &[&str] = &[
    "package.accept_keywords",
    "package.keywords",
    "package.license",
    "package.mask",
    "package.unmask",
    "package.use",
    "package.env",
    "package.properties",
];

/// Rewrite the atoms in `/etc/portage/package.*`, `profile/package.*`, and
/// `sets/*` for the given cp `moves` and `slotmoves`, honoring leading `-`/`*`
/// prefixes and annotating each change. Returns the number of files changed.
pub fn update_config_files(
    config_dir: &Path,
    moves: &[(String, String)],
    slotmoves: &[(String, String, String)],
    interner: &Interner,
) -> Result<usize> {
    let mut changed = 0;
    let mut files: Vec<PathBuf> = Vec::new();
    for name in CONFIG_TARGETS {
        collect_config_files(&config_dir.join(name), &mut files);
        collect_config_files(&config_dir.join("profile").join(name), &mut files);
    }
    let sets_dir = config_dir.join("sets");
    if sets_dir.is_dir() {
        collect_config_files(&sets_dir, &mut files);
    }

    for file in files {
        if rewrite_config_file(&file, moves, slotmoves, interner)? {
            changed += 1;
        }
    }
    Ok(changed)
}

/// Collect a config target that may be a single file or a directory of files.
fn collect_config_files(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        if let Ok(read) = std::fs::read_dir(path) {
            for entry in read.flatten() {
                if entry.path().is_file() {
                    out.push(entry.path());
                }
            }
        }
    } else if path.is_file() {
        out.push(path.to_path_buf());
    }
}

/// Rewrite one config file's atom lines, annotating each change. Returns whether
/// the file changed.
fn rewrite_config_file(
    path: &Path,
    moves: &[(String, String)],
    slotmoves: &[(String, String, String)],
    interner: &Interner,
) -> Result<bool> {
    let body = std::fs::read_to_string(path).map_err(|e| InstallError::io(path, e))?;
    let features = features_for_level(8);
    let mut out = String::new();
    let mut changed = false;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        // A line is `<atom> <rest...>`; strip a leading `-`/`*` from the atom.
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let atom_tok = parts.next().unwrap_or("");
        let rest = parts.next();
        let (prefix, bare) = split_prefix(atom_tok);

        let mut new_bare = bare.to_string();
        for (old, new) in moves {
            new_bare = rewrite_dep_cp(&new_bare, old, new, "", features, interner);
        }
        for (cp, os, ns) in slotmoves {
            new_bare = rewrite_dep_slot(&new_bare, cp, os, ns, features, interner);
        }

        if new_bare != bare {
            changed = true;
            out.push_str(&format!(
                "# updated by package move: {bare} -> {new_bare}\n"
            ));
            out.push_str(prefix);
            out.push_str(&new_bare);
            if let Some(rest) = rest {
                out.push(' ');
                out.push_str(rest);
            }
            out.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if changed {
        write_atomic(path, out.as_bytes())?;
    }
    Ok(changed)
}

/// Split a leading `-` (incremental removal) or `*` (system atom) prefix off an
/// atom token.
fn split_prefix(token: &str) -> (&str, &str) {
    if let Some(rest) = token.strip_prefix('-') {
        ("-", rest)
    } else if let Some(rest) = token.strip_prefix('*') {
        ("*", rest)
    } else {
        ("", token)
    }
}

/// Create parents and write `bytes` to `path` atomically.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| InstallError::io(parent, e))?;
    }
    moraine_common::fs::atomic_write(path, bytes)
        .map_err(|e| InstallError::io(path, std::io::Error::other(e.to_string())))
}

/// Map a VDB error into an install error.
fn vdb_err(e: moraine_vdb::VdbError) -> InstallError {
    InstallError::Realize {
        cpv: "global-update".to_string(),
        reason: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_rewrite_honors_prefixes_and_annotates() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path();
        let pkg_use = etc.join("package.use");
        std::fs::write(
            &pkg_use,
            "# comment\ndev-util/foo ssl\n-dev-util/foo zlib\n*dev-util/foo\nother/keep flag\n",
        )
        .unwrap();
        let i = Interner::new();
        let changed = update_config_files(
            etc,
            &[("dev-util/foo".into(), "dev-libs/foo".into())],
            &[],
            &i,
        )
        .unwrap();
        assert_eq!(changed, 1);
        let body = std::fs::read_to_string(&pkg_use).unwrap();
        assert!(body.contains("dev-libs/foo ssl"));
        assert!(
            body.contains("-dev-libs/foo zlib"),
            "incremental prefix kept"
        );
        assert!(body.contains("*dev-libs/foo"), "system prefix kept");
        assert!(body.contains("# updated by package move"));
        assert!(body.contains("other/keep flag"), "unrelated line untouched");
    }

    #[test]
    fn world_rename_swaps_cp() {
        let dir = tempfile::tempdir().unwrap();
        let world = dir.path().join("world");
        std::fs::write(&world, "dev-util/foo\napp-misc/bar\n").unwrap();
        let i = Interner::new();
        assert!(rename_world(&world, "dev-util/foo", "dev-libs/foo", &i).unwrap());
        let body = std::fs::read_to_string(&world).unwrap();
        assert!(body.contains("dev-libs/foo"));
        assert!(!body.contains("dev-util/foo"));
        assert!(body.contains("app-misc/bar"));
    }
}
