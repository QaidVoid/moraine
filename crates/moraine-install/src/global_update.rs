//! Global package-move replay (the post-sync `_global_updates` pass).
//!
//! After a changed sync, this reads each synced repository's `profiles/updates/`
//! directives and replays them across the authoritative `/var/db/pkg` tree and
//! its `installed.mvdb` cache, the world favorites file, `/etc/portage/package.*`,
//! and the writable local binary-package directory and its `Packages` index, so a
//! renamed or re-slotted package is referred to by its current name everywhere.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use moraine_atom::{rewrite_dep_cp, rewrite_dep_slot};
use moraine_binpkg::{PackagesIndex, moves::rename_local_artifact};
use moraine_common::Interner;
use moraine_eapi::features_for_level;
use moraine_merge::{ConfigProtect, variant_name};
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

/// A list of `(old_cp, new_cp)` rename directives.
type Moves = Vec<(String, String)>;
/// A list of `(cp, old_slot, new_slot)` slotmove directives.
type SlotMoves = Vec<(String, String, String)>;

/// A move/slotmove resolved to strings, tagged with the repository it came from.
struct Resolved {
    repo: String,
    moves: Moves,
    slotmoves: SlotMoves,
}

/// The writable local binhost being rewritten in place.
struct LocalBinhost {
    dir: PathBuf,
    index: PackagesIndex,
    changed: bool,
}

/// Replay the package moves of the synced `repos` (each `(name, path)`, in search
/// order with masters first) across the installed `store`, the authoritative
/// `vdb_root` tree, the `world_path` file, the `config_dir` (`/etc/portage`), and
/// the writable local `pkgdir` and its `Packages` index. `config_protect` routes
/// protected config rewrites through the configuration-protection mechanism.
/// `mtimes` gates which update files are applied; the caller persists the returned
/// `applied_files` mtimes only after the whole pass commits.
#[allow(clippy::too_many_arguments)]
pub fn global_update(
    store: &mut Store,
    repos: &[(String, PathBuf)],
    world_path: &Path,
    config_dir: &Path,
    vdb_root: &Path,
    pkgdir: Option<&Path>,
    config_protect: &ConfigProtect,
    mtimes: &BTreeMap<PathBuf, i64>,
) -> Result<GlobalUpdateReport> {
    let mut report = GlobalUpdateReport::default();
    let interner = store.interner().clone();

    // The repositories that ship a `profiles/updates/` directory, and the master
    // repository (the first in the masters-first search order), used by the
    // originating-repository gate.
    let repo_map: BTreeSet<String> = repos
        .iter()
        .filter(|(_, path)| path.join("profiles/updates").is_dir())
        .map(|(name, _)| name.clone())
        .collect();
    let master_repo: Option<String> = repos.first().map(|(name, _)| name.clone());

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
            let (cmds, errors) = parse_updates(&text, &interner);
            for cmd in cmds {
                push_command(&interner, cmd, &mut moves, &mut slotmoves);
            }
            // A malformed file's mtime is not recorded, so it is re-read and its
            // errors re-reported next run; its valid directives still applied.
            if errors.is_empty() {
                report.applied_files.push((file.path, file.mtime));
            } else {
                for err in &errors {
                    eprintln!(
                        "warning: {}: invalid update line `{}`: {}",
                        file.path.display(),
                        err.line,
                        err.reason
                    );
                }
            }
        }
        if !moves.is_empty() || !slotmoves.is_empty() {
            resolved.push(Resolved {
                repo: name.clone(),
                moves,
                slotmoves,
            });
        }
    }

    let master = master_repo.as_deref();

    // Phase 1: world rewrites, seeing the pre-move installed set.
    for r in &resolved {
        let pred = repo_predicate(&r.repo, master, &repo_map);
        for (old, new) in &r.moves {
            if world_match_cp(store, &interner, &[old, new], &pred) {
                let changed = rewrite_world(world_path, |line| {
                    rewrite_dep_cp(line, old, new, "", features_for_level(8), &interner)
                })?;
                report.world_renames += changed as usize;
            }
        }
        for (cp, os, ns) in &r.slotmoves {
            if world_match_cp(store, &interner, &[cp], &pred) {
                let changed = rewrite_world(world_path, |line| {
                    rewrite_dep_slot(line, cp, os, ns, features_for_level(8), &interner)
                })?;
                report.world_renames += changed as usize;
            }
        }
    }

    // The writable local binhost, when present.
    let mut binhost = load_local_binhost(pkgdir);
    let mut export_cpvs: BTreeSet<String> = BTreeSet::new();
    let mut removed_cpvs: Vec<String> = Vec::new();

    // Phase 2: per-repo VDB and binpkg moves/slotmoves.
    for r in &resolved {
        let pred = repo_predicate(&r.repo, master, &repo_map);
        for (old, new) in &r.moves {
            let changed = store.move_ent(old, new, &pred).map_err(vdb_err)?;
            report.vdb_renames += changed.len();
            for c in changed {
                if let Some(rm) = c.removed_cpv {
                    removed_cpvs.push(rm);
                }
                export_cpvs.insert(c.cpv);
            }
            if let Some(b) = binhost.as_mut() {
                apply_binpkg_move(b, old, new, &interner);
            }
        }
        for (cp, os, ns) in &r.slotmoves {
            let changed = store.move_slot_ent(cp, os, ns, &pred).map_err(vdb_err)?;
            report.vdb_slotmoves += changed.len();
            for c in changed {
                export_cpvs.insert(c.cpv);
            }
            if let Some(b) = binhost.as_mut() {
                let moved = b.index.move_slot_ent(cp, os, ns, &interner);
                if moved > 0 {
                    b.changed = true;
                }
            }
        }
    }

    // Phase 3: config files, gated like the world rewrite.
    let (gated_moves, gated_slotmoves) =
        gated_config_directives(store, &interner, &resolved, master, &repo_map);
    report.config_files_changed += update_config_files(
        config_dir,
        &gated_moves,
        &gated_slotmoves,
        &interner,
        config_protect,
    )?;

    // Phase 4: per-repo dependency rewrites across installed and binary packages.
    for r in &resolved {
        let pred = repo_predicate(&r.repo, master, &repo_map);
        let changed = store
            .update_ents(&r.moves, &r.slotmoves, &pred)
            .map_err(vdb_err)?;
        report.dep_rewrites += changed.len();
        for c in changed {
            export_cpvs.insert(c.cpv);
        }
        if let Some(b) = binhost.as_mut() {
            for entry in &mut b.index.packages {
                let self_cp = cpv_cp(&entry.cpv).unwrap_or_default();
                moraine_binpkg::moves::rewrite_dep_keys(
                    &mut entry.metadata,
                    &r.moves,
                    &r.slotmoves,
                    &self_cp,
                    &interner,
                );
            }
            b.changed = true;
        }
    }

    // Mirror the cache mutations onto the authoritative dbdir tree: remove renamed
    // sources, then re-export every changed record so its on-disk files match.
    for old_cpv in &removed_cpvs {
        if let Some((cat, pkg, ver)) = split_cpv(old_cpv) {
            moraine_vdb::vardb::remove_record(vdb_root, &cat, &pkg, &ver).map_err(vdb_err)?;
        }
    }
    for cpv in &export_cpvs {
        if let Some(record) = store.records().iter().find(|r| r.cpv(&interner) == *cpv) {
            let ebuild = read_dbdir_ebuild(vdb_root, record, &interner);
            moraine_vdb::vardb::export_record(vdb_root, record, &interner, ebuild.as_deref())
                .map_err(vdb_err)?;
        }
    }

    // Write the rewritten local index back when anything changed.
    if let Some(b) = binhost.as_ref()
        && b.changed
    {
        let text = b.index.emit(&interner);
        let path = b.dir.join("Packages");
        moraine_common::fs::atomic_write(&path, text.as_bytes())
            .map_err(|e| InstallError::io(&path, std::io::Error::other(e.to_string())))?;
    }

    Ok(report)
}

/// The originating-repository gate Portage uses: a record matches when its
/// repository equals the update file's repository, or when the update file
/// belongs to the master repository and the record's repository is empty or names
/// a repository absent from the synced update set.
fn repo_predicate<'a>(
    repo_name: &'a str,
    master_repo: Option<&'a str>,
    repo_map: &'a BTreeSet<String>,
) -> impl Fn(Option<&str>) -> bool + 'a {
    move |record_repo| {
        let repository = record_repo.unwrap_or("");
        repository == repo_name
            || (master_repo == Some(repo_name) && !repo_map.contains(repository))
    }
}

/// Whether the installed store holds a record of any of `cps` that passes the
/// originating-repository gate, the condition for a world or config rewrite.
fn world_match_cp(
    store: &Store,
    interner: &Interner,
    cps: &[&str],
    match_repo: &dyn Fn(Option<&str>) -> bool,
) -> bool {
    store.records().iter().any(|r| {
        let cp = format!(
            "{}/{}",
            interner.resolve(r.category).unwrap_or_default(),
            interner.resolve(r.package).unwrap_or_default()
        );
        cps.contains(&cp.as_str()) && {
            let resolved = r.repository.and_then(|s| interner.resolve(s));
            match_repo(resolved.as_deref())
        }
    })
}

/// Collect the gated `move`/`slotmove` directives for the config-file rewrite:
/// only directives whose best installed match passes the originating-repository
/// gate are applied, matching `_config_repo_match`.
fn gated_config_directives(
    store: &Store,
    interner: &Interner,
    resolved: &[Resolved],
    master: Option<&str>,
    repo_map: &BTreeSet<String>,
) -> (Moves, SlotMoves) {
    let mut moves = Vec::new();
    let mut slotmoves = Vec::new();
    for r in resolved {
        let pred = repo_predicate(&r.repo, master, repo_map);
        for (old, new) in &r.moves {
            if world_match_cp(store, interner, &[old, new], &pred) {
                moves.push((old.clone(), new.clone()));
            }
        }
        for (cp, os, ns) in &r.slotmoves {
            if world_match_cp(store, interner, &[cp], &pred) {
                slotmoves.push((cp.clone(), os.clone(), ns.clone()));
            }
        }
    }
    (moves, slotmoves)
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

/// Rewrite the world favorites file by applying `rewrite` to each entry, keeping
/// it sorted and unique. Returns whether anything changed.
fn rewrite_world(path: &Path, rewrite: impl Fn(&str) -> String) -> Result<bool> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(InstallError::io(path, e)),
    };
    let mut set: BTreeSet<String> = BTreeSet::new();
    let mut changed = false;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let rewritten = rewrite(trimmed);
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

/// Load the writable local binhost (its `Packages` index) when `pkgdir` is given,
/// writable, and ships a parseable `Packages` file. Returns `None` to skip the
/// binary-package rewrite cleanly otherwise.
fn load_local_binhost(pkgdir: Option<&Path>) -> Option<LocalBinhost> {
    let dir = pkgdir?;
    if !pkgdir_writable(dir) {
        return None;
    }
    let packages = dir.join("Packages");
    if !packages.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&packages).ok()?;
    let index = PackagesIndex::parse(&text).ok()?;
    Some(LocalBinhost {
        dir: dir.to_path_buf(),
        index,
        changed: false,
    })
}

/// Whether `pkgdir` exists and is writable, the analog of Portage's
/// `bindb.writable`.
fn pkgdir_writable(pkgdir: &Path) -> bool {
    pkgdir.is_dir()
        && std::fs::metadata(pkgdir)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false)
}

/// Apply a `move` to the local binhost: rename matching index stanzas and rename
/// the on-disk artifacts for each affected version.
fn apply_binpkg_move(binhost: &mut LocalBinhost, old_cp: &str, new_cp: &str, interner: &Interner) {
    let prefix = format!("{old_cp}-");
    let versions: Vec<String> = binhost
        .index
        .packages
        .iter()
        .filter_map(|e| e.cpv.strip_prefix(&prefix))
        .filter(|ver| ver.starts_with(|c: char| c.is_ascii_digit()))
        .map(str::to_string)
        .collect();
    let moved = binhost.index.move_ent(old_cp, new_cp, interner);
    if moved > 0 {
        binhost.changed = true;
    }
    for ver in versions {
        let _ = rename_local_artifact(
            &binhost.dir,
            &format!("{old_cp}-{ver}"),
            &format!("{new_cp}-{ver}"),
        );
    }
}

/// Read the ebuild copy already present in a record's dbdir so a re-export can
/// preserve it. Returns `None` when no `<PF>.ebuild` is present.
fn read_dbdir_ebuild(
    vdb_root: &Path,
    record: &moraine_vdb::record::PackageRecord,
    interner: &Interner,
) -> Option<Vec<u8>> {
    let dir = moraine_vdb::vardb::record_dbdir(vdb_root, record, interner);
    let read = std::fs::read_dir(&dir).ok()?;
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "ebuild") {
            return std::fs::read(&path).ok();
        }
    }
    None
}

/// Split a cpv (`category/package-version`) into its `(category, package,
/// version)` strings.
fn split_cpv(cpv: &str) -> Option<(String, String, String)> {
    let (category, rest) = cpv.split_once('/')?;
    let (pkg, version) = split_pkg_version(rest)?;
    Some((category.to_string(), pkg, version))
}

/// The `category/package` of a cpv string.
fn cpv_cp(cpv: &str) -> Option<String> {
    let (category, rest) = cpv.split_once('/')?;
    let (pkg, _) = split_pkg_version(rest)?;
    Some(format!("{category}/{pkg}"))
}

/// Split `package-version` at the version boundary.
fn split_pkg_version(pv: &str) -> Option<(String, String)> {
    let mut idx = 0;
    while let Some(rel) = pv[idx..].find('-') {
        let at = idx + rel;
        let tail = &pv[at + 1..];
        if tail.starts_with(|c: char| c.is_ascii_digit())
            && moraine_version::Version::parse(tail).is_ok()
        {
            return Some((pv[..at].to_string(), tail.to_string()));
        }
        idx = at + 1;
    }
    None
}

/// The `/etc/portage` files (and directory forms) whose atoms are rewritten,
/// matching `lib/portage/update.py` `myxfiles`.
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

/// The profile-only files whose atoms are rewritten.
const PROFILE_TARGETS: &[&str] = &[
    "package.accept_keywords",
    "package.keywords",
    "package.license",
    "package.mask",
    "package.unmask",
    "package.use",
    "package.env",
    "package.properties",
    "packages",
    "package.use.force",
    "package.use.mask",
    "package.use.stable.force",
    "package.use.stable.mask",
];

/// Rewrite the atoms in `/etc/portage/package.*`, `profile/*`, and `sets/*` for
/// the given cp `moves` and `slotmoves`, honoring leading `-`/`*` prefixes and
/// annotating each change, routing protected files through `config_protect`.
/// Returns the number of files changed.
pub fn update_config_files(
    config_dir: &Path,
    moves: &[(String, String)],
    slotmoves: &[(String, String, String)],
    interner: &Interner,
    config_protect: &ConfigProtect,
) -> Result<usize> {
    let mut changed = 0;
    let mut files: Vec<PathBuf> = Vec::new();
    for name in CONFIG_TARGETS {
        collect_config_files(&config_dir.join(name), &mut files);
    }
    for name in PROFILE_TARGETS {
        collect_config_files(&config_dir.join("profile").join(name), &mut files);
    }
    let sets_dir = config_dir.join("sets");
    if sets_dir.is_dir() {
        collect_config_files(&sets_dir, &mut files);
    }

    for file in files {
        if rewrite_config_file(
            &file,
            config_dir,
            moves,
            slotmoves,
            interner,
            config_protect,
        )? {
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
/// the file changed. A protected file is written to a `._cfgNNNN_` variant rather
/// than overwriting the live file.
fn rewrite_config_file(
    path: &Path,
    config_dir: &Path,
    moves: &[(String, String)],
    slotmoves: &[(String, String, String)],
    interner: &Interner,
    config_protect: &ConfigProtect,
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
        let dest = protected_destination(path, config_dir, config_protect);
        write_atomic(&dest, out.as_bytes())?;
    }
    Ok(changed)
}

/// The path a config rewrite is written to: the live file, or a `._cfgNNNN_`
/// variant sibling when the file's logical path is protected.
fn protected_destination(
    path: &Path,
    config_dir: &Path,
    config_protect: &ConfigProtect,
) -> PathBuf {
    let logical = match path.strip_prefix(config_dir) {
        Ok(rel) => format!("/etc/portage/{}", rel.to_string_lossy()),
        Err(_) => path.to_string_lossy().into_owned(),
    };
    if !config_protect.is_protected(&logical) {
        return path.to_path_buf();
    }
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    let target_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let existing: Vec<String> = std::fs::read_dir(parent)
        .map(|read| {
            read.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    parent.join(variant_name(&target_name, &existing))
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
    use moraine_merge::ConfigProtect;

    fn no_protect() -> ConfigProtect {
        ConfigProtect::new(Vec::<String>::new(), Vec::<String>::new())
    }

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
            &no_protect(),
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
    fn profile_packages_star_atom_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path();
        let packages = etc.join("profile").join("packages");
        std::fs::create_dir_all(packages.parent().unwrap()).unwrap();
        std::fs::write(&packages, "*dev-util/foo\n").unwrap();
        let i = Interner::new();
        let changed = update_config_files(
            etc,
            &[("dev-util/foo".into(), "dev-libs/foo".into())],
            &[],
            &i,
            &no_protect(),
        )
        .unwrap();
        assert_eq!(changed, 1);
        let body = std::fs::read_to_string(&packages).unwrap();
        assert!(body.contains("*dev-libs/foo"));
    }

    #[test]
    fn protected_rewrite_lands_in_cfg_variant() {
        let dir = tempfile::tempdir().unwrap();
        let etc = dir.path();
        let pkg_use = etc.join("package.use");
        std::fs::write(&pkg_use, "dev-util/foo ssl\n").unwrap();
        let i = Interner::new();
        // Protect /etc so the live file is preserved and a variant is written.
        let protect = ConfigProtect::new(["/etc".to_string()], Vec::<String>::new());
        let changed = update_config_files(
            etc,
            &[("dev-util/foo".into(), "dev-libs/foo".into())],
            &[],
            &i,
            &protect,
        )
        .unwrap();
        assert_eq!(changed, 1);
        // The live file is untouched.
        assert_eq!(
            std::fs::read_to_string(&pkg_use).unwrap(),
            "dev-util/foo ssl\n"
        );
        // A `._cfg` variant carries the rewrite.
        let variant = etc.join("._cfg0000_package.use");
        assert!(variant.is_file(), "variant written");
        assert!(
            std::fs::read_to_string(&variant)
                .unwrap()
                .contains("dev-libs/foo ssl")
        );
    }

    #[test]
    fn world_move_swaps_cp() {
        let dir = tempfile::tempdir().unwrap();
        let world = dir.path().join("world");
        std::fs::write(&world, "dev-util/foo\napp-misc/bar\n").unwrap();
        let i = Interner::new();
        let changed = rewrite_world(&world, |line| {
            rewrite_dep_cp(
                line,
                "dev-util/foo",
                "dev-libs/foo",
                "",
                features_for_level(8),
                &i,
            )
        })
        .unwrap();
        assert!(changed);
        let body = std::fs::read_to_string(&world).unwrap();
        assert!(body.contains("dev-libs/foo"));
        assert!(!body.contains("dev-util/foo"));
        assert!(body.contains("app-misc/bar"));
    }

    #[test]
    fn world_slotmove_reslots_entry() {
        let dir = tempfile::tempdir().unwrap();
        let world = dir.path().join("world");
        std::fs::write(&world, "dev-lang/python:3.4\n").unwrap();
        let i = Interner::new();
        let changed = rewrite_world(&world, |line| {
            rewrite_dep_slot(
                line,
                "dev-lang/python",
                "3.4",
                "3.11",
                features_for_level(8),
                &i,
            )
        })
        .unwrap();
        assert!(changed);
        let body = std::fs::read_to_string(&world).unwrap();
        assert!(body.contains("dev-lang/python:3.11"));
    }
}
