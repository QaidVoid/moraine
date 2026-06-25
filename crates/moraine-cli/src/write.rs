//! The write-path actions: unmerge, depclean, prune, config-update, and sync.
//!
//! Each action assembles its inputs from the loaded configuration and the
//! installed store, then drives the orchestrator or the relevant engine. The
//! dangerous live-filesystem writes happen inside `moraine-merge` via the
//! orchestrator's [`EngineApplier`]; this module only plans the work, confirms
//! it when asked, and reports the outcome.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use miette::{IntoDiagnostic, Result, miette};
use moraine_install::{
    BinpkgRunner, EngineApplier, InstallTask, LocalPkgdir, PendingUpdate, Realized, Resolution,
    StepRunner, Transaction, TransactionEngine, depclean_orphans, prune_superseded, resolve_update,
    would_break_retained,
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
    runtime_deps: Vec<String>,
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
pub(crate) fn merge_context(ctx: &ConfigContext, wr: &WriteRoots) -> MergeContext {
    MergeContext {
        eroot: wr.eroot.clone(),
        vdb_dir: wr.vdb_dir.clone(),
        state_dir: wr.state_dir.clone(),
        features: Features::from_tokens(ctx.features.iter().map(String::as_str)),
        config_protect: ConfigProtect::new(
            ctx.config_protect.clone(),
            ctx.config_protect_mask.clone(),
        ),
    }
}

/// Read installed packages from the store under `vdb_dir`.
fn read_installed(vdb_dir: &Path) -> Result<Vec<Installed>> {
    let store = Store::load(StorePaths::in_dir(vdb_dir)).into_diagnostic()?;
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
        for kind in [DependKind::RDepend, DependKind::PDepend] {
            if let Some(dep) = record.depends.get(kind) {
                runtime_deps.extend(dep_cps(&dep.raw));
            }
        }
        out.push(Installed {
            cpv,
            cp,
            slot,
            version: record.version.clone(),
            runtime_deps,
        });
    }
    Ok(out)
}

/// Run a removal transaction over the given uninstall tasks.
fn run_removal(cli: &Cli, ctx: &ConfigContext, wr: &WriteRoots, cpvs: &[String]) -> Result<()> {
    if cpvs.is_empty() {
        println!("Nothing to remove.");
        return Ok(());
    }
    println!("The following packages would be unmerged:");
    for cpv in cpvs {
        println!("  {cpv}");
    }
    if cli.pretend {
        return Ok(());
    }
    if !confirm(cli.ask) {
        println!("Operation cancelled.");
        return Ok(());
    }

    let tasks: Vec<InstallTask> = cpvs
        .iter()
        .map(|cpv| {
            let cp = cp_of_cpv(cpv);
            InstallTask::uninstall(cpv.clone(), cp, String::new())
        })
        .collect();

    ensure_dirs(wr)?;
    let mctx = merge_context(ctx, wr);
    let applier = EngineApplier::new(mctx);
    let runner = NoBuild;
    let engine = TransactionEngine::new(&runner, &applier, &wr.state_dir);
    engine
        .run(&Transaction::new(tasks))
        .map_err(|e| miette!("unmerge failed: {e}"))?;
    println!("Removed {} package(s).", cpvs.len());
    Ok(())
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
    let mctx = merge_context(ctx, &wr);
    let applier = EngineApplier::new(mctx);
    let engine = TransactionEngine::new(&runner, &applier, &wr.state_dir);
    engine.resume().map_err(|e| miette!("resume failed: {e}"))?;
    println!("Resume complete.");
    Ok(())
}

/// Unmerge explicitly named packages.
pub fn unmerge(cli: &Cli, ctx: &ConfigContext, roots: &Roots) -> Result<()> {
    let wr = WriteRoots::from(roots);
    let installed = read_installed(&wr.vdb_dir)?;
    let wanted: BTreeSet<String> = cli.targets.iter().map(|t| cp_of_atom(t)).collect();
    let matched: Vec<String> = installed
        .iter()
        .filter(|p| wanted.contains(&p.cp))
        .map(|p| p.cpv.clone())
        .collect();
    if matched.is_empty() {
        println!("No installed packages matched.");
        return Ok(());
    }
    run_removal(cli, ctx, &wr, &matched)
}

/// Remove packages not needed by the world or system sets.
pub fn depclean(cli: &Cli, ctx: &ConfigContext, roots: &Roots) -> Result<()> {
    let wr = WriteRoots::from(roots);
    let installed = read_installed(&wr.vdb_dir)?;
    let pkgs: Vec<moraine_install::InstalledPackage> = installed
        .iter()
        .map(|p| moraine_install::InstalledPackage {
            cpv: p.cpv.clone(),
            cp: p.cp.clone(),
            slot: p.slot.clone(),
            version: p.version.clone(),
            deps: p.runtime_deps.clone(),
        })
        .collect();
    let roots_set: BTreeSet<String> = ctx
        .world
        .iter()
        .chain(ctx.system.iter())
        .map(|a| cp_of_atom(a))
        .collect();
    let orphans = depclean_orphans(&pkgs, &roots_set);

    let removed: BTreeSet<String> = orphans.cpvs.iter().cloned().collect();
    if would_break_retained(&pkgs, &removed) {
        return Err(miette!(
            "refusing depclean: removal would leave a retained package unsatisfied"
        ));
    }
    run_removal(cli, ctx, &wr, &orphans.cpvs)
}

/// Remove installed versions superseded within a slot.
pub fn prune(cli: &Cli, ctx: &ConfigContext, roots: &Roots) -> Result<()> {
    let wr = WriteRoots::from(roots);
    let installed = read_installed(&wr.vdb_dir)?;
    let pkgs: Vec<moraine_install::InstalledPackage> = installed
        .iter()
        .map(|p| moraine_install::InstalledPackage {
            cpv: p.cpv.clone(),
            cp: p.cp.clone(),
            slot: p.slot.clone(),
            version: p.version.clone(),
            deps: p.runtime_deps.clone(),
        })
        .collect();
    let superseded = prune_superseded(&pkgs);
    run_removal(cli, ctx, &wr, &superseded.cpvs)
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
        RepoRefresher, RevisionHistory, SyncEngine, SystemRunner, default_registry,
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
    let refresher = RepoRefresher::new(&repo_set, &store_dir);
    let engine = SyncEngine::new(&repo_set, &registry, &refresher, &runner, &staging);

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
    Ok(())
}

/// Collect `._cfgNNNN_` config variants directly under `dir`.
fn collect_variants(dir: &Path, out: &mut Vec<PendingUpdate>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_variants(&path, out);
        } else if let Some(update) = PendingUpdate::from_variant(&path) {
            out.push(update);
        }
    }
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
}
