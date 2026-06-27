//! The write-path actions: unmerge, depclean, prune, config-update, and sync.
//!
//! Each action assembles its inputs from the loaded configuration and the
//! installed store, then drives the orchestrator or the relevant engine. The
//! dangerous live-filesystem writes happen inside `moraine-merge` via the
//! orchestrator's [`EngineApplier`]; this module only plans the work, confirms
//! it when asked, and reports the outcome.

use std::collections::{BTreeSet, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use miette::{IntoDiagnostic, Result, miette};
use moraine_atom::{Atom, PackageRef};
use moraine_install::{
    BinpkgRunner, EngineApplier, InstallTask, InstalledPackage, LocalPkgdir, PendingUpdate,
    Realized, Resolution, StepRunner, Transaction, TransactionEngine, WorldUpdate,
    depclean_orphans, depclean_targeted, prune_superseded, resolve_update, would_break_retained,
};
use moraine_merge::{ConfigProtect, Features, MergeContext};
use moraine_vdb::record::DependKind;
use moraine_vdb::store::{Store, StorePaths};
use moraine_version::Version;

use crate::args::Cli;
use crate::config::{ConfigContext, Roots};

/// A [`StepRunner`] for removal-only transactions, where no task is ever
/// realized into a build. Realizing is a programming error here.
struct NoBuild;

impl StepRunner for NoBuild {
    fn realize(&self, task: &InstallTask) -> moraine_install::Result<Realized> {
        Err(moraine_install::InstallError::Realize {
            cpv: task.cpv.clone(),
            reason: "removal transaction does not build packages".to_owned(),
        })
    }
}

/// One installed package as read from the store.
struct Installed {
    cpv: String,
    cp: String,
    slot: String,
    version: Version,
    /// The `category/package` of each runtime dependency (RDEPEND, PDEPEND,
    /// IDEPEND).
    runtime_deps: Vec<String>,
    /// The `category/package` of each build dependency (DEPEND, BDEPEND).
    build_deps: Vec<String>,
}

/// The live-system roots the write actions operate against.
pub(crate) struct WriteRoots {
    pub(crate) eroot: PathBuf,
    pub(crate) vdb_dir: PathBuf,
    pub(crate) state_dir: PathBuf,
}

impl WriteRoots {
    pub(crate) fn from(roots: &Roots) -> Self {
        let eroot = roots.root_dir();
        WriteRoots {
            vdb_dir: eroot.join("var/db/pkg"),
            state_dir: eroot.join("var/lib/portage"),
            eroot,
        }
    }
}

/// Ensure the installed-store and state directories exist before a transaction.
pub(crate) fn ensure_dirs(wr: &WriteRoots) -> Result<()> {
    std::fs::create_dir_all(&wr.vdb_dir).into_diagnostic()?;
    std::fs::create_dir_all(&wr.state_dir).into_diagnostic()?;
    Ok(())
}

/// Build the merge context from configuration and roots.
///
/// `noconfmem` comes from the `--noconfmem` command-line flag and forces a fresh
/// `._cfg` variant for a differing protected config regardless of config memory.
pub(crate) fn merge_context(ctx: &ConfigContext, wr: &WriteRoots, noconfmem: bool) -> MergeContext {
    MergeContext {
        eroot: wr.eroot.clone(),
        vdb_dir: wr.vdb_dir.clone(),
        state_dir: wr.state_dir.clone(),
        features: Features::from_tokens(ctx.features.iter().map(String::as_str)),
        config_protect: ConfigProtect::with_root(
            &wr.eroot,
            ctx.config_protect.clone(),
            ctx.config_protect_mask.clone(),
        ),
        collision_ignore: whitespace_list(ctx.vars.get("COLLISION_IGNORE")),
        uninstall_ignore: whitespace_list(ctx.vars.get("UNINSTALL_IGNORE")),
        install_mask: install_mask_from(ctx),
        noconfmem,
    }
}

/// Build the combined INSTALL_MASK filter from configuration and the
/// `nodoc`/`noman`/`noinfo` FEATURES.
fn install_mask_from(ctx: &ConfigContext) -> moraine_merge::install_mask::InstallMask {
    let features: Vec<&str> = ctx.features.iter().map(String::as_str).collect();
    let spec = moraine_merge::install_mask::combined_spec(
        ctx.vars.get("INSTALL_MASK").unwrap_or_default(),
        ctx.vars.get("PKG_INSTALL_MASK").unwrap_or_default(),
        ctx.vars.get("EPREFIX").unwrap_or_default(),
        &features,
    );
    moraine_merge::install_mask::InstallMask::new(&spec)
}

/// Split a whitespace-separated configuration value into owned tokens.
fn whitespace_list(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// Load the installed store under `vdb_dir`, importing the classic Portage vdb
/// (the `category/package` directories under `/var/db/pkg`) when moraine's own
/// `installed.mvdb` is empty, so existing installs are visible. The import is
/// persisted when the store is writable (root) and otherwise kept in memory.
pub(crate) fn load_installed_store(vdb_dir: &Path) -> Result<Store> {
    match Store::load(StorePaths::in_dir(vdb_dir)) {
        Ok(store) if !store.records().is_empty() => Ok(revalidate_cache(store, vdb_dir)),
        // An empty cache: derive it from the authoritative directory tree.
        Ok(store) => Ok(rebuild_cache_from_tree(vdb_dir)?.unwrap_or(store)),
        // The cache format changed: the `/var/db/pkg` tree is authoritative, so
        // rebuild the cache from it rather than failing.
        Err(moraine_vdb::VdbError::UnsupportedVersion { found, expected }) => {
            tracing::info!(
                found,
                expected,
                "vdb cache format changed; rebuilding from tree"
            );
            match rebuild_cache_from_tree(vdb_dir)? {
                Some(store) => Ok(store),
                None => Ok(Store::empty(StorePaths::in_dir(vdb_dir))),
            }
        }
        Err(e) => Err(e).into_diagnostic(),
    }
}

/// Validate each cached record against its authoritative dbdir's modification
/// time, re-importing any package whose dbdir changed and dropping any whose
/// dbdir vanished, leaving unchanged packages served from the cache. Mirrors
/// `aux_get`'s per-package `os.stat` comparison. On no change the store is
/// returned untouched.
fn revalidate_cache(store: Store, vdb_dir: &Path) -> Store {
    let interner = store.interner().clone();
    let mut stale = false;
    let mut records = Vec::with_capacity(store.records().len());
    for rec in store.records() {
        let dir = moraine_vdb::vardb::record_dbdir(vdb_dir, rec, &interner);
        if moraine_vdb::vardb::dbdir_mtime(&dir) == rec.dbdir_mtime {
            records.push(rec.clone());
            continue;
        }
        stale = true;
        // The dbdir is gone (an external unmerge, or the old half of an external
        // same-slot upgrade): drop the cache entry. This is checked explicitly
        // rather than relying on a re-import error, since a missing `SLOT` now
        // defaults to `0` and would otherwise yield a degenerate record.
        if !dir.is_dir() {
            continue;
        }
        match moraine_vdb::import::import_package_dir(&dir, &interner) {
            Ok(reimported) => records.push(reimported),
            Err(_) => continue,
        }
    }

    // Discover dbdirs present in the tree but absent from the cache, for example
    // an external `emerge` install or the new half of an external same-slot
    // upgrade, mirroring `_iter_cpv_all` enumerating the tree on every access
    // (`vartree.py:525-571`).
    let known: HashSet<PathBuf> = records
        .iter()
        .map(|rec| moraine_vdb::vardb::record_dbdir(vdb_dir, rec, &interner))
        .collect();
    match moraine_vdb::list_package_dirs(vdb_dir) {
        Ok(dirs) => {
            for dir in dirs {
                if known.contains(&dir) {
                    continue;
                }
                match moraine_vdb::import::import_package_dir(&dir, &interner) {
                    Ok(record) => {
                        records.push(record);
                        stale = true;
                    }
                    Err(error) => tracing::warn!(
                        package = %dir.display(),
                        %error,
                        "skipping malformed package directory during discovery"
                    ),
                }
            }
        }
        Err(error) => tracing::warn!(%error, "could not enumerate vdb tree for discovery"),
    }

    if !stale {
        return store;
    }
    tracing::info!("revalidated vdb cache against changed dbdirs");
    let imported = Store::from_records(StorePaths::in_dir(vdb_dir), interner, records);
    let _ = imported.write_primary();
    imported
}

/// Rebuild the `installed.mvdb` cache from the Portage-format `/var/db/pkg` tree.
/// Returns `None` when the tree holds no records.
fn rebuild_cache_from_tree(vdb_dir: &Path) -> Result<Option<Store>> {
    let interner = std::sync::Arc::new(moraine_common::Interner::new());
    let records = match moraine_vdb::import_vdb(vdb_dir, &interner) {
        Ok(records) if !records.is_empty() => records,
        _ => return Ok(None),
    };
    tracing::info!(
        count = records.len(),
        "rebuilt vdb cache from directory tree"
    );
    let imported = Store::from_records(StorePaths::in_dir(vdb_dir), interner, records);
    // Best effort: persist so later runs load it directly.
    let _ = imported.write_primary();
    Ok(Some(imported))
}

/// Read installed packages from the store under `vdb_dir`.
fn read_installed(vdb_dir: &Path) -> Result<Vec<Installed>> {
    let store = load_installed_store(vdb_dir)?;
    let interner = store.interner();
    let mut out = Vec::new();
    for record in store.records() {
        let cpv = record.cpv(interner);
        let category = interner.resolve(record.category).unwrap_or_default();
        let package = interner.resolve(record.package).unwrap_or_default();
        let cp = format!("{category}/{package}");
        let slot = interner
            .resolve(record.slot.slot)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let mut runtime_deps = Vec::new();
        for kind in [
            DependKind::RDepend,
            DependKind::PDepend,
            DependKind::IDepend,
        ] {
            if let Some(dep) = record.depends.get(kind) {
                runtime_deps.extend(dep_cps(&dep.raw));
            }
        }
        let mut build_deps = Vec::new();
        for kind in [DependKind::Depend, DependKind::BDepend] {
            if let Some(dep) = record.depends.get(kind) {
                build_deps.extend(dep_cps(&dep.raw));
            }
        }
        out.push(Installed {
            cpv,
            cp,
            slot,
            version: record.version.clone(),
            runtime_deps,
            build_deps,
        });
    }
    Ok(out)
}

/// Run a removal transaction over the given uninstall tasks.
///
/// Returns `true` only when packages were actually unmerged, that is not under
/// `--pretend`, not cancelled, and not an empty set. The caller uses this to
/// gate the world-set deselection.
fn run_removal(cli: &Cli, ctx: &ConfigContext, wr: &WriteRoots, cpvs: &[String]) -> Result<bool> {
    if cpvs.is_empty() {
        println!("Nothing to remove.");
        return Ok(false);
    }
    println!("The following packages would be unmerged:");
    for cpv in cpvs {
        println!("  {cpv}");
    }
    if cli.pretend {
        return Ok(false);
    }
    if !confirm(cli.ask) {
        println!("Operation cancelled.");
        return Ok(false);
    }

    let tasks: Vec<InstallTask> = cpvs
        .iter()
        .map(|cpv| {
            let cp = cp_of_cpv(cpv);
            InstallTask::uninstall(cpv.clone(), cp, String::new())
        })
        .collect();

    ensure_dirs(wr)?;
    let mctx = merge_context(ctx, wr, cli.noconfmem);
    let applier = EngineApplier::new(mctx);
    let runner = NoBuild;
    let engine = TransactionEngine::new(&runner, &applier, &wr.state_dir);
    engine
        .run(&Transaction::new(tasks))
        .map_err(|e| miette!("unmerge failed: {e}"))?;
    println!("Removed {} package(s).", cpvs.len());
    Ok(true)
}

/// Resume the unfinished portion of the most recent transaction.
pub fn resume(cli: &Cli, ctx: &ConfigContext, roots: &Roots) -> Result<()> {
    let wr = WriteRoots::from(roots);
    if !moraine_install::has_pending(&wr.state_dir) {
        println!("No transaction to resume.");
        return Ok(());
    }
    if cli.pretend {
        println!("A transaction is pending and would be resumed.");
        return Ok(());
    }
    ensure_dirs(&wr)?;
    let pkgdir = wr.eroot.join("var/cache/binpkgs");
    let stage = wr.state_dir.join("install-stage");
    let runner = BinpkgRunner::new(LocalPkgdir { pkgdir }, stage);
    let mctx = merge_context(ctx, &wr, cli.noconfmem);
    let applier = EngineApplier::new(mctx);
    let engine =
        TransactionEngine::new(&runner, &applier, &wr.state_dir).with_keep_going(cli.keep_going);
    engine.resume().map_err(|e| miette!("resume failed: {e}"))?;
    println!("Resume complete.");
    Ok(())
}

/// Unmerge explicitly named packages.
///
/// Each target is parsed as a precise atom and matched against the installed
/// records, honoring the version operator, version, and slot, so `=cat/pkg-1.2`
/// removes only that version and `cat/pkg:2` removes only slot `2`, while a bare
/// `cat/pkg` still matches every installed version. Mirrors Portage's
/// `vartree.dbapi.match` selection.
pub fn unmerge(cli: &Cli, ctx: &ConfigContext, roots: &Roots) -> Result<()> {
    let wr = WriteRoots::from(roots);
    let store = load_installed_store(&wr.vdb_dir)?;
    let matched = match_unmerge_cpvs(&store, &cli.targets);
    if matched.is_empty() {
        println!("No installed packages matched.");
        return Ok(());
    }
    run_removal(cli, ctx, &wr, &matched)?;
    Ok(())
}

/// Select the installed `cpv`s matched by the unmerge `targets`.
///
/// Each target is parsed as a precise atom against the store interner so its
/// symbols compare equal to the records', then every installed record whose
/// category, package, version, and slot satisfy the atom is selected. A bare
/// `cat/pkg` matches every installed version; `=cat/pkg-1.2` or `cat/pkg:2`
/// matches only the named version or slot. Mirrors `vartree.dbapi.match`.
fn match_unmerge_cpvs(store: &Store, targets: &[String]) -> Vec<String> {
    let interner = store.interner();
    let atoms: Vec<Atom> = targets
        .iter()
        .filter_map(|t| Atom::parse(t, moraine_eapi::PERMISSIVE, interner).ok())
        .collect();
    store
        .records()
        .iter()
        .filter(|record| {
            let pref = PackageRef {
                category: record.category,
                package: record.package,
                version: &record.version,
                slot: Some(record.slot.slot),
                subslot: record.slot.subslot,
                repo: record.repository,
            };
            atoms.iter().any(|atom| atom.matches(&pref))
        })
        .map(|record| record.cpv(interner))
        .collect()
}

/// Remove packages not needed by the world or system sets.
///
/// Reachability spans each retained package's runtime and build dependencies by
/// default; `--with-bdeps=n` excludes the build dependencies. When target atoms
/// are named, removal is restricted to the named `category/package` keys and
/// every unmatched installed package is protected.
pub fn depclean(cli: &Cli, ctx: &ConfigContext, roots: &Roots) -> Result<()> {
    let wr = WriteRoots::from(roots);
    let installed = read_installed(&wr.vdb_dir)?;
    let pkgs = installed_packages(&installed, with_bdeps(cli));
    let roots_set: BTreeSet<String> = ctx
        .world
        .iter()
        .chain(ctx.system.iter())
        .map(|a| cp_of_atom(a))
        .collect();
    let orphans = if cli.targets.is_empty() {
        depclean_orphans(&pkgs, &roots_set)
    } else {
        let targets: BTreeSet<String> = cli.targets.iter().map(|t| cp_of_atom(t)).collect();
        depclean_targeted(&pkgs, &roots_set, &targets)
    };

    let removed: BTreeSet<String> = orphans.cpvs.iter().cloned().collect();
    if would_break_retained(&pkgs, &removed) {
        return Err(miette!(
            "refusing depclean: removal would leave a retained package unsatisfied"
        ));
    }
    if run_removal(cli, ctx, &wr, &orphans.cpvs)? {
        deselect_removed(&wr, &pkgs, &orphans.cpvs)?;
    }
    Ok(())
}

/// Remove installed versions superseded by a higher version of the same
/// `category/package`, keeping the single highest version across all slots.
pub fn prune(cli: &Cli, ctx: &ConfigContext, roots: &Roots) -> Result<()> {
    let wr = WriteRoots::from(roots);
    let installed = read_installed(&wr.vdb_dir)?;
    let pkgs = installed_packages(&installed, with_bdeps(cli));
    let superseded = prune_superseded(&pkgs);
    // Keep any lower version whose removal would leave a retained package with an
    // unsatisfied dependency.
    let to_remove: Vec<String> = superseded
        .cpvs
        .into_iter()
        .filter(|cpv| {
            let one: BTreeSet<String> = std::iter::once(cpv.clone()).collect();
            !would_break_retained(&pkgs, &one)
        })
        .collect();
    if run_removal(cli, ctx, &wr, &to_remove)? {
        deselect_removed(&wr, &pkgs, &to_remove)?;
    }
    Ok(())
}

/// Whether build-time dependencies are considered during removal. They are by
/// default and excluded only by `--with-bdeps=n`, matching Portage's
/// `bdeps=auto` for the removal actions.
fn with_bdeps(cli: &Cli) -> bool {
    cli.with_bdeps.as_deref() != Some("n")
}

/// Map the installed packages into the removal planner's model, building each
/// package's dependency set from its runtime dependencies plus, unless excluded,
/// its build dependencies.
fn installed_packages(installed: &[Installed], with_bdeps: bool) -> Vec<InstalledPackage> {
    installed
        .iter()
        .map(|p| {
            let mut deps = p.runtime_deps.clone();
            if with_bdeps {
                deps.extend(p.build_deps.iter().cloned());
            }
            InstalledPackage {
                cpv: p.cpv.clone(),
                cp: p.cp.clone(),
                slot: p.slot.clone(),
                version: p.version.clone(),
                deps,
            }
        })
        .collect()
}

/// Deselect from the world set each removed package's `category/package` that has
/// no surviving installed version, mirroring `cleanPackage`. A pruned lower
/// version is never deselected because its higher version survives.
fn deselect_removed(wr: &WriteRoots, pkgs: &[InstalledPackage], removed: &[String]) -> Result<()> {
    let remove = world_deselect(pkgs, removed);
    if remove.is_empty() {
        return Ok(());
    }
    WorldUpdate {
        add: Vec::new(),
        remove,
    }
    .apply(&wr.state_dir.join("world"), false)
    .map_err(|e| miette!("world update failed: {e}"))?;
    Ok(())
}

/// The `category/package` keys to drop from the world set after a removal: each
/// removed package's key that has no surviving installed version.
fn world_deselect(pkgs: &[InstalledPackage], removed: &[String]) -> Vec<String> {
    let removed_set: BTreeSet<&str> = removed.iter().map(String::as_str).collect();
    let surviving: BTreeSet<&str> = pkgs
        .iter()
        .filter(|p| !removed_set.contains(p.cpv.as_str()))
        .map(|p| p.cp.as_str())
        .collect();
    let mut out: Vec<String> = pkgs
        .iter()
        .filter(|p| removed_set.contains(p.cpv.as_str()) && !surviving.contains(p.cp.as_str()))
        .map(|p| p.cp.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Resolve pending CONFIG_PROTECT updates left under the live root.
pub fn config_update(cli: &Cli, ctx: &ConfigContext, roots: &Roots) -> Result<()> {
    let wr = WriteRoots::from(roots);
    let mut pending = Vec::new();
    for prefix in &ctx.config_protect {
        let dir = wr.eroot.join(prefix.trim_start_matches('/'));
        collect_variants(&dir, &mut pending);
    }
    if pending.is_empty() {
        println!("No pending config updates.");
        return Ok(());
    }
    println!("{} pending config update(s).", pending.len());
    if cli.pretend {
        for update in &pending {
            println!("  {}", update.variant.display());
        }
        return Ok(());
    }
    for update in &pending {
        println!("Config update for {}", update.target.display());
        let resolution = if cli.ask {
            prompt_resolution()
        } else {
            Resolution::Apply
        };
        let changed = resolve_update(update, resolution).map_err(|e| miette!("{e}"))?;
        println!("  {}", if changed { "applied" } else { "kept existing" });
    }
    Ok(())
}

/// Synchronize the configured repositories.
pub fn sync(cli: &Cli, roots: &Roots) -> Result<()> {
    use moraine_repo::discover;
    use moraine_sync::{
        ExtrasMap, RepoRefresher, RevisionHistory, SyncEngine, SystemRunner, default_registry,
    };

    if cli.pretend {
        println!("Would sync configured repositories.");
        return Ok(());
    }

    let wr = WriteRoots::from(roots);
    let repos_conf = roots.config_dir().join("etc/portage/repos.conf");
    let repo_set =
        discover(&repos_conf).map_err(|e| miette!("repository discovery failed: {e}"))?;

    let store_dir = wr.eroot.join("var/cache/moraine/repos");
    let staging = wr.state_dir.join("sync-staging");
    let runner = SystemRunner;
    let registry = default_registry(runner);
    // Regenerate metadata for ebuilds whose md5-cache entry is missing or stale
    // by sourcing them with a working `inherit`, instead of excluding them.
    let generator = crate::regen::EbuildMetadataGenerator::new(&repo_set);
    let mut refresher = RepoRefresher::new(&repo_set, &store_dir);
    if let Some(generator) = &generator {
        refresher = refresher.with_generator(generator);
    }
    // Load the repos.conf extras (auto-sync, post-sync, volatile) and the config
    // root so those controls and the postsync.d hooks take effect. A malformed
    // repos.conf is surfaced rather than silently reverting to clobbering.
    let extras = ExtrasMap::load(&repos_conf)
        .map_err(|e| miette!("repos.conf extras failed to load: {e}"))?;
    let engine = SyncEngine::new(&repo_set, &registry, &refresher, &runner, &staging)
        .with_extras(extras)
        .with_config_root(roots.config_dir());

    let history_path = wr.state_dir.join("sync-history.mrev");
    let mut history =
        RevisionHistory::load(&history_path).unwrap_or_else(|_| RevisionHistory::new());
    let report = if cli.targets.is_empty() {
        engine.sync_all(&mut history)
    } else {
        engine.sync_named(&cli.targets, &mut history)
    };
    let _ = history.save(&history_path);

    let mut failed = false;
    for (name, result) in &report.results {
        println!("  {name}: {result:?}");
        if let moraine_sync::RepoResult::Failed(_) = result {
            failed = true;
        }
    }
    if failed {
        return Err(miette!("one or more repositories failed to sync"));
    }

    // Replay package moves (profiles/updates) across the installed store, world,
    // and /etc/portage after a successful sync. A failure here is reported but
    // does not discard the synced tree.
    if let Err(e) = apply_package_moves(&repo_set, &wr, roots) {
        eprintln!("warning: package-move replay failed: {e}");
    }

    // Scan the freshly-synced trees for relevant unread news and report counts.
    if let Ok(ctx) = ConfigContext::load(roots) {
        crate::news_state::display_after_action(&ctx, &wr.vdb_dir, &wr.eroot, &repo_set);
    }
    Ok(())
}

/// Run the global package-move pass after a successful sync, gated by the
/// per-update-file mtime map and persisting the new mtimes only once the whole
/// pass commits.
fn apply_package_moves(
    repo_set: &moraine_repo::RepoSet,
    wr: &WriteRoots,
    roots: &Roots,
) -> Result<()> {
    let mut store = load_installed_store(&wr.vdb_dir)?;
    let repos: Vec<(String, PathBuf)> = repo_set
        .ordered()
        .map(|r| (r.name.clone(), r.location.clone()))
        .collect();
    let world_path = wr.state_dir.join("world");
    let config_dir = roots.config_dir().join("etc/portage");
    let pkgdir = wr.eroot.join("var/cache/binpkgs");
    let mtime_path = wr.state_dir.join("package-move-mtimes");
    let mtimes = moraine_repo::load_mtimes(&mtime_path);

    // The configuration-protection policy for routing protected config rewrites.
    let config_protect = match ConfigContext::load(roots) {
        Ok(ctx) => ConfigProtect::with_root(
            &wr.eroot,
            ctx.config_protect.clone(),
            ctx.config_protect_mask.clone(),
        ),
        Err(_) => ConfigProtect::default(),
    };

    let report = moraine_install::global_update(
        &mut store,
        &repos,
        &world_path,
        &config_dir,
        &wr.vdb_dir,
        Some(&pkgdir),
        &config_protect,
        &mtimes,
    )
    .map_err(|e| miette!("{e}"))?;

    // Commit the store, then record the new update-file mtimes (only after the
    // whole pass succeeds, so an interrupted pass re-runs the whole batch).
    store.compact().into_diagnostic()?;
    let mut new_mtimes = mtimes;
    for (path, mtime) in &report.applied_files {
        new_mtimes.insert(path.clone(), *mtime);
    }
    let _ = moraine_repo::store_mtimes(&mtime_path, &new_mtimes);

    let total = report.vdb_renames
        + report.vdb_slotmoves
        + report.world_renames
        + report.config_files_changed;
    if total > 0 {
        println!(
            "Applied package moves: {} renamed, {} re-slotted, {} dep rewrites, {} world, {} config files",
            report.vdb_renames,
            report.vdb_slotmoves,
            report.dep_rewrites,
            report.world_renames,
            report.config_files_changed
        );
    }
    Ok(())
}

/// Collect `._cfgNNNN_` config variants under `dir`, pruning hidden
/// dot-directories and skipping backup artifacts, matching Portage's
/// `find_updated_config_files`.
fn collect_variants(dir: &Path, out: &mut Vec<PendingUpdate>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            // Prune hidden dot-directories (`find -name '.*' -type d -prune`).
            if !name.starts_with('.') {
                collect_variants(&path, out);
            }
        } else if !is_backup_name(&name)
            && let Some(update) = PendingUpdate::from_variant(&path)
        {
            out.push(update);
        }
    }
}

/// Whether a file name is a backup artifact excluded from the pending-update
/// scan: a `~`-suffixed file or a `*.bak` (case-insensitive), matching the
/// `! -name '.*~' ! -iname '.*.bak'` filters in `find_updated_config_files`.
fn is_backup_name(name: &str) -> bool {
    name.ends_with('~') || name.to_ascii_lowercase().ends_with(".bak")
}

/// Prompt for a yes/no confirmation, defaulting to yes on an empty line. When
/// `ask` is false the action proceeds without prompting.
pub(crate) fn confirm(ask: bool) -> bool {
    if !ask {
        return true;
    }
    print!("Proceed? [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let answer = line.trim().to_ascii_lowercase();
    answer.is_empty() || answer == "y" || answer == "yes"
}

/// Prompt for how to resolve one config update.
fn prompt_resolution() -> Resolution {
    print!("  [a]pply new, [k]eep existing? [a/K] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Resolution::Keep;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "a" | "apply" => Resolution::Apply,
        _ => Resolution::Keep,
    }
}

/// The `category/package` head of an atom string, stripping operators, slot, and
/// any version.
pub(crate) fn cp_of_atom(atom: &str) -> String {
    let trimmed = atom.trim_start_matches(['>', '<', '=', '~', '!']);
    let no_slot = trimmed.split(':').next().unwrap_or(trimmed);
    match no_slot.split_once('/') {
        Some((cat, rest)) => format!("{cat}/{}", strip_version(rest)),
        None => no_slot.to_owned(),
    }
}

/// The `category/package` of a `category/package-version` string.
fn cp_of_cpv(cpv: &str) -> String {
    match cpv.split_once('/') {
        Some((cat, rest)) => format!("{cat}/{}", strip_version(rest)),
        None => cpv.to_owned(),
    }
}

/// Extract `category/package` heads from a raw `*DEPEND` string by tokenizing
/// and keeping atom-shaped tokens. Group and conditional tokens are skipped.
fn dep_cps(raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .filter(|t| t.contains('/') && !t.ends_with('?') && *t != "(" && *t != ")")
        .map(cp_of_atom)
        .collect()
}

/// Strip a trailing `-<version>` from a package-name segment, where a version
/// starts with a digit after a hyphen.
fn strip_version(name_version: &str) -> String {
    let bytes = name_version.as_bytes();
    let mut idx = 0;
    while let Some(pos) = name_version[idx..].find('-') {
        let at = idx + pos;
        if at + 1 < bytes.len() && bytes[at + 1].is_ascii_digit() {
            return name_version[..at].to_owned();
        }
        idx = at + 1;
    }
    name_version.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cp_strips_operators_and_version() {
        assert_eq!(cp_of_atom(">=dev-libs/openssl-3.0.1"), "dev-libs/openssl");
        assert_eq!(cp_of_atom("dev-libs/openssl:0/3"), "dev-libs/openssl");
        assert_eq!(cp_of_atom("!sys-apps/foo"), "sys-apps/foo");
    }

    #[test]
    fn cpv_head_drops_version() {
        assert_eq!(cp_of_cpv("dev-libs/openssl-3.0.1-r1"), "dev-libs/openssl");
        assert_eq!(cp_of_cpv("app-misc/hello-2.10"), "app-misc/hello");
    }

    #[test]
    fn strip_version_finds_version_boundary() {
        assert_eq!(strip_version("openssl-3.0.1"), "openssl");
        assert_eq!(strip_version("foo-bar-1.2"), "foo-bar");
        assert_eq!(strip_version("no-version"), "no-version");
    }

    #[test]
    fn dep_cps_extracts_atoms() {
        let raw = "|| ( dev-libs/a >=dev-libs/b-1.2 ) sys-apps/c";
        let cps = dep_cps(raw);
        assert!(cps.contains(&"dev-libs/a".to_owned()));
        assert!(cps.contains(&"dev-libs/b".to_owned()));
        assert!(cps.contains(&"sys-apps/c".to_owned()));
    }

    #[test]
    fn changed_dbdir_reimports_only_that_package() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let vdb = dir.path();
        // Two installed packages, both recorded as SLOT 0.
        for pkg in ["a", "b"] {
            let pdir = vdb.join("cat").join(format!("{pkg}-1"));
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(pdir.join("SLOT"), "0\n").unwrap();
            std::fs::write(pdir.join("EAPI"), "8\n").unwrap();
            std::fs::write(pdir.join("COUNTER"), "1\n").unwrap();
        }

        let interner = Arc::new(moraine_common::Interner::new());
        let mut records = moraine_vdb::import_vdb(vdb, &interner).unwrap();
        // Mark `cat/a-1` stale by storing a wrong dbdir mtime, and change its
        // on-disk SLOT so a re-import is observable. `cat/b-1` keeps its correct
        // mtime; its on-disk SLOT is changed too, so serving from cache (no
        // re-import) is observable.
        for rec in &mut records {
            if rec.cpv(&interner) == "cat/a-1" {
                rec.dbdir_mtime = 1;
            }
        }
        std::fs::write(vdb.join("cat/a-1/SLOT"), "5\n").unwrap();
        std::fs::write(vdb.join("cat/b-1/SLOT"), "9\n").unwrap();

        let store = Store::from_records(StorePaths::in_dir(vdb), interner, records);
        store.write_primary().unwrap();

        let reloaded = load_installed_store(vdb).unwrap();
        let li = reloaded.interner();
        let slot_of = |cpv: &str| {
            reloaded
                .records()
                .iter()
                .find(|r| r.cpv(li) == cpv)
                .map(|r| li.resolve(r.slot.slot).unwrap().to_string())
                .unwrap()
        };
        // `cat/a-1` was re-imported from the changed dbdir.
        assert_eq!(slot_of("cat/a-1"), "5");
        // `cat/b-1` was served from the cache, not re-imported.
        assert_eq!(slot_of("cat/b-1"), "0");
    }

    #[test]
    fn externally_added_dbdir_is_discovered_on_load() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let vdb = dir.path();
        let mk = |name: &str, counter: &str| {
            let pdir = vdb.join("cat").join(name);
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(pdir.join("SLOT"), "0\n").unwrap();
            std::fs::write(pdir.join("EAPI"), "8\n").unwrap();
            std::fs::write(pdir.join("COUNTER"), counter).unwrap();
        };

        // Seed the cache with one installed package and persist it.
        mk("pkg-1", "1\n");
        let interner = Arc::new(moraine_common::Interner::new());
        let records = moraine_vdb::import_vdb(vdb, &interner).unwrap();
        let store = Store::from_records(StorePaths::in_dir(vdb), interner, records);
        store.write_primary().unwrap();

        // After the cache was written, an external emerge installs a brand-new
        // package and performs a same-slot upgrade of the seeded one.
        mk("dep-1", "2\n");
        std::fs::remove_dir_all(vdb.join("cat/pkg-1")).unwrap();
        mk("pkg-2", "3\n");

        let reloaded = load_installed_store(vdb).unwrap();
        let li = reloaded.interner();
        let cpvs: BTreeSet<String> = reloaded.records().iter().map(|r| r.cpv(li)).collect();
        // The externally-added package is discovered on load.
        assert!(cpvs.contains("cat/dep-1"));
        // The new half of the same-slot upgrade is visible; the old half is gone.
        assert!(cpvs.contains("cat/pkg-2"));
        assert!(!cpvs.contains("cat/pkg-1"));
    }

    #[test]
    fn world_deselect_drops_sole_version_keeps_surviving() {
        let mk = |cpv: &str, cp: &str, ver: &str| InstalledPackage {
            cpv: cpv.to_owned(),
            cp: cp.to_owned(),
            slot: "0".to_owned(),
            version: Version::parse(ver).unwrap(),
            deps: Vec::new(),
        };
        let pkgs = vec![
            mk("dev-lang/python-3.10", "dev-lang/python", "3.10"),
            mk("dev-lang/python-3.11", "dev-lang/python", "3.11"),
            mk("app/sole-1", "app/sole", "1"),
        ];
        // Removing the sole version of `app/sole` deselects it; removing the
        // lower python slot does not, because a higher version survives.
        let removed = vec!["app/sole-1".to_owned(), "dev-lang/python-3.10".to_owned()];
        assert_eq!(world_deselect(&pkgs, &removed), vec!["app/sole".to_owned()]);
    }

    /// Build an in-memory store from a `(package-version, slot)` list under a
    /// throwaway vdb tree. The tree is dropped on return; matching only reads the
    /// loaded records.
    fn store_from_tree(pkgs: &[(&str, &str)]) -> Store {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let vdb = dir.path();
        for (pkg, slot) in pkgs {
            let pdir = vdb.join("cat").join(pkg);
            std::fs::create_dir_all(&pdir).unwrap();
            std::fs::write(pdir.join("SLOT"), format!("{slot}\n")).unwrap();
            std::fs::write(pdir.join("EAPI"), "8\n").unwrap();
            std::fs::write(pdir.join("COUNTER"), "1\n").unwrap();
        }
        let interner = Arc::new(moraine_common::Interner::new());
        let records = moraine_vdb::import_vdb(vdb, &interner).unwrap();
        Store::from_records(StorePaths::in_dir(vdb), interner, records)
    }

    #[test]
    fn unmerge_versioned_atom_selects_only_that_version() {
        let store = store_from_tree(&[("pkg-1.2", "0"), ("pkg-1.3", "0")]);
        // A precise versioned atom matches only the named version.
        assert_eq!(
            match_unmerge_cpvs(&store, &["=cat/pkg-1.2".to_owned()]),
            vec!["cat/pkg-1.2".to_owned()]
        );
    }

    #[test]
    fn unmerge_slot_atom_selects_only_that_slot() {
        let store = store_from_tree(&[("pkg-1", "1"), ("pkg-2", "2")]);
        // A slot atom matches only the version in that slot.
        assert_eq!(
            match_unmerge_cpvs(&store, &["cat/pkg:2".to_owned()]),
            vec!["cat/pkg-2".to_owned()]
        );
        // A bare cp still matches every installed version.
        let mut all = match_unmerge_cpvs(&store, &["cat/pkg".to_owned()]);
        all.sort();
        assert_eq!(all, vec!["cat/pkg-1".to_owned(), "cat/pkg-2".to_owned()]);
    }

    #[test]
    fn is_backup_name_matches_tilde_and_bak() {
        assert!(is_backup_name("._cfg0000_foo.conf~"));
        assert!(is_backup_name("foo.bak"));
        assert!(is_backup_name("foo.BAK"));
        assert!(!is_backup_name("._cfg0000_foo.conf"));
    }

    #[test]
    fn collect_variants_prunes_hidden_dirs_and_backups() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("._cfg0000_foo.conf"), b"a").unwrap();
        // Backup artifacts are skipped.
        std::fs::write(root.join("._cfg0000_foo.conf~"), b"a").unwrap();
        std::fs::write(root.join("._cfg0001_foo.conf.bak"), b"a").unwrap();
        // A hidden dot-directory is pruned, so its variant is not reported.
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join(".hidden/._cfg0000_bar.conf"), b"b").unwrap();
        // A normal subdirectory is still descended into.
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/._cfg0000_baz.conf"), b"c").unwrap();

        let mut pending = Vec::new();
        collect_variants(root, &mut pending);
        let variants: Vec<String> = pending
            .iter()
            .map(|p| {
                p.variant
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert!(variants.contains(&"._cfg0000_foo.conf".to_owned()));
        assert!(variants.contains(&"._cfg0000_baz.conf".to_owned()));
        assert!(
            !variants
                .iter()
                .any(|v| v.ends_with('~') || v.to_lowercase().ends_with(".bak"))
        );
        assert!(!variants.contains(&"._cfg0000_bar.conf".to_owned()));
    }
}
