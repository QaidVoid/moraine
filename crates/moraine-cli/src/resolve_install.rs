//! The dependency-resolving install pipeline.
//!
//! This wires the real resolver into the CLI: it builds the repository index and
//! installed store against one shared interner, assembles a [`ResolvedConfig`]
//! against the same interner so masking and USE actually apply, runs the solver,
//! serializes the merge order, and drives the orchestrator. Source tasks build
//! through a [`BuildPlanner`] that turns a repository entry plus configuration
//! into a build request; binary tasks install from local packages.
//!
//! Resolving and source building only fully validate against a real repository
//! tree, so the end-to-end run is corpus-gated; the assembly here is unit-tested
//! through its smaller helpers.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::io::{IsTerminal as _, Write as _};

use miette::{Result, miette};
use moraine_build::{
    BuildRequest, ConfigEnv, FetchConfig, Manifest, NamespaceSupport, PackageIdent, PackageSpec,
    SystemRunner, srcuri,
};
use moraine_common::Interner;
use moraine_config::{ResolvedConfig, resolve_config};
use moraine_install::{
    BinpkgRunner, BinpkgSource, BuildOptions, BuildPlanner, EngineApplier, InstallError,
    InstallTask, LocalPkgdir, Realized, SourceKind, SourceRunner, StepRunner, Transaction,
    TransactionEngine,
};
use moraine_repo::store::{StoredEntry, read_entries};
use moraine_repo::{LoadedStore, RepoIndex, RepoSet, RepoStore, build_index_with, discover};
use moraine_resolve::{
    Modifiers, RealSource, Task, TaskKind as ResolveTaskKind, resolve_with, serialize,
};
use moraine_vdb::store::Store;
use moraine_version::Version;

use crate::args::Cli;
use crate::config::{ConfigContext, Roots};
use crate::plan::build_plan;
use crate::render::{Operation, render_merge_list, render_tree};
use crate::write::{WriteRoots, cp_of_atom, ensure_dirs, merge_context};

/// The binary-package preferences in effect, combining the CLI switches with the
/// `make.conf` `FEATURES` tokens (`getbinpkg`, `buildpkg`).
struct BinaryPrefs {
    getbinpkg: bool,
    usepkg: bool,
    buildpkg: bool,
    buildpkgonly: bool,
}

impl BinaryPrefs {
    fn from(cli: &Cli, features: &[String]) -> Self {
        let has = |name: &str| features.iter().any(|f| f == name);
        BinaryPrefs {
            // `getbinpkg` also implies considering binary packages, like emerge.
            getbinpkg: cli.getbinpkg || has("getbinpkg"),
            usepkg: cli.usepkg || cli.getbinpkg || has("getbinpkg"),
            buildpkg: cli.buildpkg || has("buildpkg"),
            buildpkgonly: cli.buildpkgonly,
        }
    }
}

/// Run the dependency-resolving install over the command-line targets.
pub fn run(cli: &Cli, ctx: &ConfigContext, roots: &Roots) -> Result<()> {
    let wr = WriteRoots::from(roots);
    let config_dir = roots.config_dir();
    let interner = Arc::new(Interner::new());

    // Build the resolver inputs against one shared interner so masking and USE
    // from the configuration compare equal to the repository's symbols.
    let repos_conf = config_dir.join("etc/portage/repos.conf");
    let (repo_index, store_dir) = obtain_index(&repos_conf, &wr.eroot, &interner)?;
    if repo_index
        .repos()
        .iter()
        .all(|r| r.store.entries().is_empty())
    {
        return Err(miette!(
            "the repository index is empty; run `moraine --sync` first"
        ));
    }
    let repo_set =
        discover(&repos_conf).map_err(|e| miette!("repository discovery failed: {e}"))?;
    let vdb = crate::write::load_installed_store(&wr.vdb_dir)?;
    let repo_masks = crate::config::repo_mask_inputs(&repo_set);
    let config = resolve_config(
        &ctx.profile,
        &ctx.vars,
        &config_dir,
        &repo_masks,
        ctx.system.clone(),
        ctx.world.clone(),
        &interner,
    );

    // Compute `@preserved-rebuild` from the preserved-libs registry and the
    // installed soname data, but only when that set is actually requested.
    let ctx_owned;
    let ctx: &ConfigContext = if cli.targets.iter().any(|t| t == "@preserved-rebuild") {
        let mut owned = ctx.clone();
        owned.preserved_rebuild = compute_preserved_rebuild(&vdb, &wr.state_dir);
        ctx_owned = owned;
        &ctx_owned
    } else {
        ctx
    };

    // Resolve each bare package name to a category, then expand sets and atoms.
    let qualified = qualify_targets(&cli.targets, &repo_index)?;
    let request = crate::sets::expand(
        ctx,
        &qualified,
        &cli.exclude,
        crate::run::modifiers_from(cli),
    )?;
    if request.atoms.is_empty() {
        println!("No targets to install.");
        return Ok(());
    }

    // Binary preferences and the binhost index, loaded before resolution so the
    // resolver can prefer a version that has a binary package (`getbinpkg`),
    // matching `emerge`. Reused afterwards for display and execution.
    let prefs = BinaryPrefs::from(cli, &ctx.features);
    let pkgdir = wr.eroot.join("var/cache/binpkgs");
    let binhost = if prefs.getbinpkg {
        let uris = crate::binhost::binhost_uris(&config_dir, &ctx.vars);
        let binhost_cache = store_dir
            .parent()
            .map(|p| p.join("binhost"))
            .unwrap_or_else(|| store_dir.join("binhost"));
        // Reuse the cached index; `--sync` refreshes it.
        crate::binhost::IndexedBinhost::load(
            &uris,
            moraine_binpkg::fetch::FetchCommand::default(),
            &binhost_cache,
            cli.sync,
        )
    } else {
        None
    };
    let binary_cpvs = binary_cpv_set(&pkgdir, binhost.as_ref());

    // Resolve and serialize the merge order, timing the solve.
    let source = RealSource::new(&repo_index, &vdb, &config).with_binaries(binary_cpvs);
    let atom_refs: Vec<&str> = request.atoms.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    let modifiers = Modifiers {
        update: request.update,
        deep: request.deep,
        newuse: request.newuse,
        changed_deps: cli.changed_deps,
        changed_slot: cli.changed_slot,
    };
    let solution = resolve_with(&source, &atom_refs, modifiers)
        .map_err(|e| miette!("resolution failed:\n{e}"))?;
    let elapsed = started.elapsed();
    println!(
        "Dependency resolution took {:.2} s (backtracks: {})",
        elapsed.as_secs_f64(),
        solution.backtracks
    );
    let raw_order = serialize(&solution).map_err(|e| miette!("merge ordering failed: {e}"))?;

    // Drop blocker uninstalls for packages that are not installed, and resolve
    // genuine ones to the real installed version(s).
    let installed = installed_versions(&vdb);
    let order = clean_order(&raw_order, &installed);
    // Drop no-op reinstalls of installed dependencies and set members, matching
    // Portage's default of not reinstalling unchanged packages. A package named
    // explicitly on the command line is still re-merged (a `[R]` reinstall), as
    // `emerge` does, even when it is already installed. Heads are taken from the
    // qualified targets so a bare name like `xmlto` matches `app-text/xmlto`.
    let explicit: BTreeSet<String> = qualified
        .iter()
        .filter(|t| !t.starts_with('@'))
        .map(|t| cp_of_atom(t))
        .collect();
    let order: Vec<Task> = order
        .into_iter()
        .filter(|task| explicit.contains(&task.cp) || !is_noop_merge(task, &solution))
        .collect();
    if order.is_empty() {
        println!("Nothing to do; the targets are already satisfied.");
        return Ok(());
    }

    let stage = wr.state_dir.join("install-stage");

    // Build the presentation plan and enrich it with source/binary, repository,
    // and download size, then render it `emerge`-style.
    let mut plan = build_plan(&order, &solution, &source);
    enrich_plan(
        &mut plan,
        &repo_set,
        &store_dir,
        &config,
        &interner,
        &prefs,
        &pkgdir,
        binhost.as_ref(),
    );
    let use_expand = moraine_config::use_expand_groups(&ctx.vars);
    for entry in &mut plan.entries {
        crate::render::apply_use_expand_groups(&mut entry.use_flags, &use_expand);
    }
    print!("{}", render_merge_list(&plan, cli.is_verbose()));
    if cli.show_tree() {
        print!("{}", render_tree(&plan, cli.is_verbose()));
    }
    print!(
        "{}",
        crate::render::render_autounmask(&solution.autounmask, &plan, &solution.edges, &explicit)
    );

    if cli.pretend {
        return Ok(());
    }
    if !crate::write::confirm(cli.ask) {
        println!("Operation cancelled.");
        return Ok(());
    }

    // Convert to orchestrator tasks, choosing source or binary per task.
    let explicit = explicit_heads(cli);
    let tasks: Vec<InstallTask> = order
        .iter()
        .map(|task| {
            to_install_task(
                task,
                &explicit,
                &prefs,
                cli.oneshot,
                &pkgdir,
                binhost.as_ref(),
            )
        })
        .collect();

    // Drive the orchestrator with a runner that dispatches source vs binary.
    ensure_dirs(&wr)?;
    let slot_bindings: HashMap<String, Vec<(String, String, Option<String>)>> = solution
        .packages
        .iter()
        .map(|p| {
            let bindings = p
                .slot_bindings
                .iter()
                .map(|b| (b.dependency.clone(), b.slot.clone(), b.subslot.clone()))
                .collect();
            (p.cpv(), bindings)
        })
        .collect();
    let planner = CliPlanner {
        repo_set: &repo_set,
        store_dir: store_dir.clone(),
        config: &config,
        ctx,
        eroot: wr.eroot.clone(),
        interner: Arc::clone(&interner),
        cache: RefCell::new(HashMap::new()),
        slot_bindings,
    };
    let command_runner = SystemRunner;
    let options = BuildOptions {
        buildpkg: prefs.buildpkg,
        buildpkgonly: prefs.buildpkgonly,
        pkgdir: pkgdir.clone(),
        ..BuildOptions::default()
    };
    // Try a local package first, then the binhost.
    let mut sources: Vec<Box<dyn BinpkgSource>> = vec![Box::new(LocalPkgdir {
        pkgdir: pkgdir.clone(),
    })];
    if let Some(bh) = binhost {
        sources.push(Box::new(bh));
    }
    let binpkg_source = crate::binhost::ChainSource::new(sources);
    let runner = CombinedRunner {
        source: SourceRunner::new(planner, &command_runner, options),
        binpkg: BinpkgRunner::new(binpkg_source, stage),
    };
    let applier = EngineApplier::new(merge_context(ctx, &wr));
    let engine = TransactionEngine::new(&runner, &applier, &wr.state_dir);
    engine
        .run(&Transaction::new(tasks))
        .map_err(|e| miette!("install failed: {e}"))?;
    println!("Installation complete.");
    Ok(())
}

/// A runner that dispatches each task to the source or binary path.
struct CombinedRunner<'a> {
    source: SourceRunner<'a, CliPlanner<'a>, SystemRunner>,
    binpkg: BinpkgRunner<crate::binhost::ChainSource>,
}

impl StepRunner for CombinedRunner<'_> {
    fn realize(&self, task: &InstallTask) -> moraine_install::Result<Realized> {
        match task.source {
            SourceKind::Source => self.source.realize(task),
            SourceKind::Binary => match self.binpkg.realize(task) {
                // No binary package was available anywhere: fall back to a source
                // build, matching `emerge`'s behavior without `--usepkgonly`.
                Err(InstallError::Realize { reason, .. })
                    if reason.contains("no compatible binary package") =>
                {
                    tracing::warn!(cpv = %task.cpv, "no binary package found; building from source");
                    self.source.realize(task)
                }
                other => other,
            },
        }
    }
}

/// Builds a [`BuildRequest`] from the repository entry and resolved config.
struct CliPlanner<'a> {
    repo_set: &'a RepoSet,
    store_dir: PathBuf,
    config: &'a ResolvedConfig,
    ctx: &'a ConfigContext,
    eroot: PathBuf,
    interner: Arc<Interner>,
    cache: RefCell<HashMap<String, Arc<Vec<StoredEntry>>>>,
    /// Resolved `:=` slot bindings per `cpv`, as `(dependency_cp, slot,
    /// subslot)`, so each merge bakes its linked slot into the recorded
    /// `*DEPEND`.
    slot_bindings: HashMap<String, Vec<(String, String, Option<String>)>>,
}

impl BuildPlanner for CliPlanner<'_> {
    fn plan(&self, task: &InstallTask) -> moraine_install::Result<BuildRequest> {
        let entry = self
            .find_entry(&task.cpv)
            .ok_or_else(|| InstallError::Realize {
                cpv: task.cpv.clone(),
                reason: "no repository entry matches the resolved package".to_owned(),
            })?;
        let location = self
            .repo_set
            .get(&entry.repository)
            .map(|r| r.location.clone())
            .ok_or_else(|| InstallError::Realize {
                cpv: task.cpv.clone(),
                reason: format!("repository `{}` has no known location", entry.repository),
            })?;

        let (category, pf) = split_cpv(&task.cpv);
        let (pn, pvr) = split_pf(&pf);
        let pkg_dir = location.join(&category).join(&pn);
        let ident = package_ident(&category, &pf, &pn, &pvr, &entry.eapi, &entry.repository);

        let mut reduced_meta = std::collections::BTreeMap::new();
        for (key, value) in [
            ("DEPEND", &entry.depend),
            ("RDEPEND", &entry.rdepend),
            ("BDEPEND", &entry.bdepend),
            ("PDEPEND", &entry.pdepend),
            ("IDEPEND", &entry.idepend),
            ("LICENSE", &entry.license),
        ] {
            if !value.trim().is_empty() {
                reduced_meta.insert(key.to_owned(), value.clone());
            }
        }

        let package = PackageSpec {
            ident,
            ebuild_path: pkg_dir.join(format!("{pf}.ebuild")),
            src_uri: entry.src_uri.clone(),
            defined_phases: entry.defined_phases.clone(),
            restrict: entry.restrict.clone(),
            slot: entry.slot.clone(),
            subslot: entry.subslot.clone(),
            iuse: entry.iuse.clone(),
            keywords: entry.keywords.clone(),
            inherited: entry.inherited.clone(),
            reduced_meta,
            manifest_path: pkg_dir.join("Manifest"),
        };

        let use_flags = self.resolved_use(&entry);
        let run_tests = self.ctx.features.iter().any(|f| f == "test")
            && !entry.restrict.iter().any(|r| r == "test");

        Ok(BuildRequest {
            package,
            config: self.config_env(),
            use_flags,
            fetch: self.fetch_config(),
            run_tests,
            require_digest: true,
            namespace_support: NamespaceSupport::default(),
            slot_bindings: self
                .slot_bindings
                .get(&task.cpv)
                .cloned()
                .unwrap_or_default(),
        })
    }
}

impl CliPlanner<'_> {
    /// Find the repository entry whose `category/package-version` equals `cpv`,
    /// reading each repository store from disk once and caching it.
    fn find_entry(&self, cpv: &str) -> Option<StoredEntry> {
        for cfg in self.repo_set.ordered() {
            let entries = self.entries_for(&cfg.name);
            if let Some(found) = entries
                .iter()
                .find(|e| format!("{}/{}-{}", e.category, e.package, e.version) == cpv)
            {
                return Some(found.clone());
            }
        }
        None
    }

    /// The cached stored entries for one repository, read from its store file.
    fn entries_for(&self, name: &str) -> Arc<Vec<StoredEntry>> {
        if let Some(cached) = self.cache.borrow().get(name) {
            return Arc::clone(cached);
        }
        let path = self.store_dir.join(format!("{name}.mrepo"));
        let entries = Arc::new(read_entries(&path).unwrap_or_default());
        self.cache
            .borrow_mut()
            .insert(name.to_owned(), Arc::clone(&entries));
        entries
    }

    /// The resolved USE set for the entry from the shared-interner configuration.
    fn resolved_use(&self, entry: &StoredEntry) -> HashSet<String> {
        let Ok(version) = Version::parse(&entry.version) else {
            return HashSet::new();
        };
        let pref = moraine_atom::PackageRef {
            category: self.interner.intern(&entry.category),
            package: self.interner.intern(&entry.package),
            version: &version,
            slot: Some(self.interner.intern(&entry.slot)),
            subslot: entry.subslot.as_deref().map(|s| self.interner.intern(s)),
            repo: Some(self.interner.intern(&entry.repository)),
        };
        self.config
            .effective_use(&pref, &entry.iuse, false)
            .enabled
            .into_iter()
            .collect()
    }

    /// The build-environment configuration from `make.conf`.
    fn config_env(&self) -> ConfigEnv {
        let mut vars = std::collections::BTreeMap::new();
        for (key, value) in self.ctx.vars.iter() {
            vars.insert(key.clone(), value.clone());
        }
        let mirrors = self
            .ctx
            .vars
            .get("GENTOO_MIRRORS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let root = self.eroot.to_string_lossy().into_owned();
        ConfigEnv {
            vars,
            features: self.ctx.features.clone(),
            mirrors,
            root: root.clone(),
            sysroot: root,
            eprefix: String::new(),
        }
    }

    /// The fetch configuration from `make.conf`.
    fn fetch_config(&self) -> FetchConfig {
        let distdir = self
            .ctx
            .vars
            .get("DISTDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.eroot.join("var/cache/distfiles"));
        let mirrors = self
            .ctx
            .vars
            .get("GENTOO_MIRRORS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        // `FETCHCOMMAND`/`RESUMECOMMAND` normally come from `make.globals` or
        // `make.conf`; the build engine's `wget` default is the last resort when
        // neither configures them.
        let defaults = FetchConfig::new(&distdir);
        let fetchcommand = match tokenize(self.ctx.vars.get("FETCHCOMMAND").unwrap_or_default()) {
            tokens if tokens.is_empty() => defaults.fetchcommand,
            tokens => tokens,
        };
        let resumecommand = match tokenize(self.ctx.vars.get("RESUMECOMMAND").unwrap_or_default()) {
            tokens if tokens.is_empty() => defaults.resumecommand,
            tokens => tokens,
        };
        FetchConfig {
            distdir,
            fetchcommand,
            resumecommand,
            mirrors,
            thirdparty: crate::config::thirdparty_mirrors(self.repo_set),
            resume_min_size: 350_000,
            max_attempts: 3,
        }
    }
}

/// Convert a serialized task into an orchestrator task.
fn to_install_task(
    task: &Task,
    explicit: &BTreeSet<String>,
    prefs: &BinaryPrefs,
    oneshot: bool,
    pkgdir: &Path,
    binhost: Option<&crate::binhost::IndexedBinhost>,
) -> InstallTask {
    let cpv = format!("{}-{}", task.cp, task.version);
    let kind = match task.kind {
        ResolveTaskKind::Uninstall => moraine_install::TaskKind::Uninstall,
        ResolveTaskKind::Merge => moraine_install::TaskKind::Merge,
    };
    let (binary, _) = binary_choice(&task.cp, &task.version, prefs, pkgdir, binhost);
    InstallTask {
        cpv,
        cp: task.cp.clone(),
        slot: task.slot.clone(),
        kind,
        source: if binary {
            SourceKind::Binary
        } else {
            SourceKind::Source
        },
        in_world: explicit.contains(&task.cp) && !oneshot,
        replaces: None,
    }
}

/// Decide whether a task installs a binary package, and whether that package
/// comes from a binhost (the `g` indicator). Honors `--usepkg`/`--getbinpkg` and
/// the `getbinpkg` `FEATURE`. A local package is preferred over the binhost.
/// The set of `cp-version` strings a binary package exists for: every binhost
/// index entry plus local `.gpkg` files under `pkgdir`. Fed to the resolver so
/// version selection can prefer a version that has a binary.
fn binary_cpv_set(
    pkgdir: &Path,
    binhost: Option<&crate::binhost::IndexedBinhost>,
) -> HashSet<String> {
    let mut set: HashSet<String> = binhost
        .map(|bh| bh.cpvs().map(str::to_owned).collect())
        .unwrap_or_default();
    if let Ok(categories) = std::fs::read_dir(pkgdir) {
        for cat in categories.flatten() {
            if !cat.path().is_dir() {
                continue;
            }
            let category = cat.file_name().to_string_lossy().into_owned();
            if let Ok(files) = std::fs::read_dir(cat.path()) {
                for f in files.flatten() {
                    let name = f.file_name().to_string_lossy().into_owned();
                    if let Some(pf) = name.strip_suffix(".gpkg") {
                        set.insert(format!("{category}/{pf}"));
                    }
                }
            }
        }
    }
    set
}

fn binary_choice(
    cp: &str,
    version: &str,
    prefs: &BinaryPrefs,
    pkgdir: &Path,
    binhost: Option<&crate::binhost::IndexedBinhost>,
) -> (bool, bool) {
    let (category, _) = cp.split_once('/').unwrap_or((cp, ""));
    let pf = format!("{}-{}", cp.rsplit('/').next().unwrap_or(cp), version);
    let local = pkgdir.join(category).join(format!("{pf}.gpkg")).exists();
    if (prefs.usepkg || prefs.getbinpkg) && local {
        // A local binary package is present.
        (true, false)
    } else if prefs.getbinpkg && binhost.is_some_and(|bh| bh.contains(&format!("{cp}-{version}"))) {
        // The binhost actually lists this version (the `g` flag).
        (true, true)
    } else {
        // No binary package anywhere: build from source.
        (false, false)
    }
}

/// Clean the serialized order: drop blocker uninstalls for packages that are not
/// installed, and expand genuine ones to the real installed `(version, slot)`.
/// Whether a merge task is a no-op: the package is already installed at the
/// resolved version and slot and is not being rebuilt, so it is not part of the
/// merge delta.
fn is_noop_merge(task: &Task, solution: &moraine_resolve::solution::ResolvedSolution) -> bool {
    task.kind == ResolveTaskKind::Merge
        && solution.packages.iter().any(|p| {
            p.cp == task.cp
                && p.slot == task.slot
                && p.version.as_str() == task.version
                && p.already_installed
                && !p.subslot_rebuild
        })
}

fn clean_order(order: &[Task], installed: &HashMap<String, Vec<(String, String)>>) -> Vec<Task> {
    let mut out = Vec::with_capacity(order.len());
    for task in order {
        match task.kind {
            ResolveTaskKind::Merge => out.push(task.clone()),
            ResolveTaskKind::Uninstall => {
                if !task.version.is_empty() {
                    // An atom-filtered blocker victim names its exact version and
                    // slot; remove only that entry if it is actually installed.
                    if installed.get(&task.cp).is_some_and(|vs| {
                        vs.iter()
                            .any(|(v, s)| v == &task.version && s == &task.slot)
                    }) {
                        out.push(task.clone());
                    }
                } else if let Some(versions) = installed.get(&task.cp) {
                    // An unversioned uninstall (an explicit `-C`) removes every
                    // installed version and slot of the cp.
                    for (version, slot) in versions {
                        out.push(Task {
                            kind: ResolveTaskKind::Uninstall,
                            cp: task.cp.clone(),
                            version: version.clone(),
                            slot: slot.clone(),
                            use_enabled: Vec::new(),
                        });
                    }
                }
            }
        }
    }
    out
}

/// Qualify each command-line target with a category. Sets (`@`-prefixed) and
/// already-qualified atoms pass through; a bare package name is resolved against
/// the repository index, prompting or erroring when it is ambiguous.
fn qualify_targets(targets: &[String], index: &RepoIndex) -> Result<Vec<String>> {
    targets.iter().map(|t| qualify_one(t, index)).collect()
}

/// Qualify one target.
fn qualify_one(target: &str, index: &RepoIndex) -> Result<String> {
    if target.starts_with('@') || target.contains('/') {
        return Ok(target.to_owned());
    }
    let (op, rest) = split_operator(target);
    let (pn, _) = split_pf(rest);
    let mut categories = categories_for(index, &pn);
    match categories.len() {
        0 => Err(miette!(
            "no package named `{pn}` found in any configured repository"
        )),
        1 => Ok(format!("{op}{}/{rest}", categories.remove(0))),
        _ => choose_category(&pn, &categories).map(|cat| format!("{op}{cat}/{rest}")),
    }
}

/// The categories that provide a package name, across all repositories.
fn categories_for(index: &RepoIndex, pn: &str) -> Vec<String> {
    let mut set = BTreeSet::new();
    for (store, category, package) in index.catalog() {
        let interner = store.store.interner();
        if interner.resolve(package).as_deref() == Some(pn)
            && let Some(cat) = interner.resolve(category)
        {
            set.insert(cat.to_string());
        }
    }
    set.into_iter().collect()
}

/// Resolve an ambiguous package name to a category: prompt when interactive,
/// otherwise error listing the candidates.
fn choose_category(pn: &str, categories: &[String]) -> Result<String> {
    let listed = categories
        .iter()
        .map(|c| format!("{c}/{pn}"))
        .collect::<Vec<_>>()
        .join(", ");
    if !std::io::stdin().is_terminal() {
        return Err(miette!(
            help = "qualify the name with its category, for example `cat/pkg`",
            "the package name `{pn}` is ambiguous; it is provided by: {listed}"
        ));
    }
    println!("Multiple categories provide `{pn}`:");
    for (i, category) in categories.iter().enumerate() {
        println!("  {}) {category}/{pn}", i + 1);
    }
    print!("Choose [1-{}]: ", categories.len());
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| miette!("could not read selection: {e}"))?;
    let choice: usize = line
        .trim()
        .parse()
        .ok()
        .filter(|n| (1..=categories.len()).contains(n))
        .ok_or_else(|| miette!("invalid selection"))?;
    Ok(categories[choice - 1].clone())
}

/// Split a leading version operator (`>`, `<`, `=`, `~`, `!`) off a target.
fn split_operator(target: &str) -> (&str, &str) {
    let end = target
        .find(|c: char| !matches!(c, '>' | '<' | '=' | '~' | '!'))
        .unwrap_or(target.len());
    target.split_at(end)
}

/// Enrich the presentation plan with source/binary kind, repository, and
/// download size, reading each package's stored entry from disk once.
#[allow(clippy::too_many_arguments)]
fn enrich_plan(
    plan: &mut crate::render::MergePlan,
    repo_set: &RepoSet,
    store_dir: &Path,
    config: &ResolvedConfig,
    interner: &Interner,
    prefs: &BinaryPrefs,
    pkgdir: &Path,
    binhost: Option<&crate::binhost::IndexedBinhost>,
) {
    let mut cache: HashMap<String, Arc<Vec<StoredEntry>>> = HashMap::new();
    for entry in &mut plan.entries {
        if entry.operation == Operation::Uninstall {
            continue;
        }
        let cpv = format!("{}-{}", entry.cp, entry.version);
        let (binary, fetched) = binary_choice(&entry.cp, &entry.version, prefs, pkgdir, binhost);
        entry.binary = binary;
        entry.fetched = fetched;
        if binary {
            entry.build_id = binhost.and_then(|bh| bh.build_id(&cpv));
        }

        let Some(stored) = lookup_entry(repo_set, store_dir, &mut cache, &cpv) else {
            continue;
        };
        entry.repository = Some(stored.repository.clone());

        // The resolver's enabled set is the whole effective USE; restrict the
        // displayed flags to the package's own IUSE (plus any removed flags).
        let iuse: HashSet<String> = stored
            .iuse
            .iter()
            .map(|f| f.trim_start_matches(['+', '-']).to_owned())
            .collect();
        entry
            .use_flags
            .retain(|f| f.removed || iuse.contains(&f.name));

        // Mark flags fixed by use.force/use.mask so they render parenthesized.
        if let Ok(version) = Version::parse(&stored.version) {
            let pref = moraine_atom::PackageRef {
                category: interner.intern(&stored.category),
                package: interner.intern(&stored.package),
                version: &version,
                slot: Some(interner.intern(&stored.slot)),
                subslot: stored.subslot.as_deref().map(|s| interner.intern(s)),
                repo: Some(interner.intern(&stored.repository)),
            };
            let forced = config.effective_use(&pref, &stored.iuse, false).forced;
            for flag in &mut entry.use_flags {
                flag.forced = forced.contains(&flag.name);
            }
        }

        entry.fetch_size = if binary {
            binary_size(pkgdir, &cpv).or_else(|| binhost.and_then(|b| b.size_of(&cpv)))
        } else {
            source_size(&stored, repo_set, config, interner)
        };
    }
}

/// Find the stored entry for `cpv`, reading each repository store once.
fn lookup_entry(
    repo_set: &RepoSet,
    store_dir: &Path,
    cache: &mut HashMap<String, Arc<Vec<StoredEntry>>>,
    cpv: &str,
) -> Option<StoredEntry> {
    for cfg in repo_set.ordered() {
        let entries = cache.entry(cfg.name.clone()).or_insert_with(|| {
            Arc::new(
                read_entries(store_dir.join(format!("{}.mrepo", cfg.name))).unwrap_or_default(),
            )
        });
        if let Some(found) = entries
            .iter()
            .find(|e| format!("{}/{}-{}", e.category, e.package, e.version) == cpv)
        {
            return Some(found.clone());
        }
    }
    None
}

/// The on-disk size of a local binary package, if present.
fn binary_size(pkgdir: &Path, cpv: &str) -> Option<u64> {
    let (category, pf) = split_cpv(cpv);
    let path = pkgdir.join(category).join(format!("{pf}.gpkg"));
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// The total download size of a source package's distfiles, summed from the
/// repository `Manifest` over the USE-reduced `SRC_URI`.
fn source_size(
    stored: &StoredEntry,
    repo_set: &RepoSet,
    config: &ResolvedConfig,
    interner: &Interner,
) -> Option<u64> {
    if stored.src_uri.trim().is_empty() {
        return None;
    }
    let version = Version::parse(&stored.version).ok()?;
    let pref = moraine_atom::PackageRef {
        category: interner.intern(&stored.category),
        package: interner.intern(&stored.package),
        version: &version,
        slot: Some(interner.intern(&stored.slot)),
        subslot: stored.subslot.as_deref().map(|s| interner.intern(s)),
        repo: Some(interner.intern(&stored.repository)),
    };
    let use_flags: HashSet<String> = config
        .effective_use(&pref, &stored.iuse, false)
        .enabled
        .into_iter()
        .collect();
    let features = moraine_eapi::features_for(&stored.eapi);
    let src_map = srcuri::parse_and_reduce(&stored.src_uri, &use_flags, features).ok()?;

    let location = repo_set.get(&stored.repository)?.location.clone();
    let manifest_path = location
        .join(&stored.category)
        .join(&stored.package)
        .join("Manifest");
    let manifest = Manifest::read(manifest_path).ok()?;
    let total: u64 = src_map
        .a()
        .iter()
        .filter_map(|d| manifest.dist(&d.name).map(|e| e.size))
        .sum();
    Some(total)
}

/// Map each installed `category/package` to its installed `(version, slot)`
/// pairs, used to resolve and filter blocker-driven uninstalls.
fn installed_versions(vdb: &Store) -> HashMap<String, Vec<(String, String)>> {
    let interner = vdb.interner();
    let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for record in vdb.records() {
        let category = interner.resolve(record.category).unwrap_or_default();
        let package = interner.resolve(record.package).unwrap_or_default();
        let cp = format!("{category}/{package}");
        let slot = interner
            .resolve(record.slot.slot)
            .map(|s| s.to_string())
            .unwrap_or_default();
        map.entry(cp)
            .or_default()
            .push((record.version.as_str().to_owned(), slot));
    }
    map
}

/// The `category/package` heads explicitly named on the command line (excluding
/// `@`-sets), which become world members.
fn explicit_heads(cli: &Cli) -> BTreeSet<String> {
    cli.targets
        .iter()
        .filter(|t| !t.starts_with('@'))
        .map(|t| cp_of_atom(t))
        .collect()
}

/// Compute the `@preserved-rebuild` set: installed packages requiring a soname
/// kept alive only by a preserved library, minus the preserved libraries' own
/// owners. Returns empty when the registry has no preserved libraries.
fn compute_preserved_rebuild(vdb: &Store, state_dir: &Path) -> Vec<String> {
    let registry = moraine_merge::PreservedLibs::load(&state_dir.join("preserved-libs"))
        .unwrap_or_else(|_| moraine_merge::PreservedLibs::new());
    if registry.is_empty() {
        return Vec::new();
    }
    let preserved_sonames: BTreeSet<String> = registry
        .entries()
        .iter()
        .map(|e| e.soname.clone())
        .collect();
    let preserved_owners: BTreeSet<String> = registry
        .entries()
        .iter()
        .map(|e| {
            let (category, pf) = split_cpv(&e.cpv);
            let (pn, _) = split_pf(&pf);
            format!("{category}/{pn}")
        })
        .collect();
    let interner = vdb.interner();
    let consumers: Vec<(String, Vec<String>)> = vdb
        .records()
        .iter()
        .filter_map(|record| {
            let category = interner.resolve(record.category)?;
            let package = interner.resolve(record.package)?;
            let sonames = vdb
                .required_sonames(record)
                .filter_map(|s| interner.resolve(s).map(|x| x.to_string()))
                .collect();
            Some((format!("{category}/{package}"), sonames))
        })
        .collect();
    moraine_config::sets::preserved_rebuild_set(&consumers, &preserved_sonames, &preserved_owners)
}

/// Split a `category/package-version` into `(category, pf)`.
fn split_cpv(cpv: &str) -> (String, String) {
    match cpv.split_once('/') {
        Some((c, pf)) => (c.to_owned(), pf.to_owned()),
        None => (String::new(), cpv.to_owned()),
    }
}

/// Split a `pf` (`pn-version`) into `(pn, pvr)` at the version boundary.
fn split_pf(pf: &str) -> (String, String) {
    let bytes = pf.as_bytes();
    let mut idx = 0;
    while let Some(pos) = pf[idx..].find('-') {
        let at = idx + pos;
        if at + 1 < bytes.len() && bytes[at + 1].is_ascii_digit() {
            return (pf[..at].to_owned(), pf[at + 1..].to_owned());
        }
        idx = at + 1;
    }
    (pf.to_owned(), String::new())
}

/// Build a [`PackageIdent`] from the split package identity.
fn package_ident(
    category: &str,
    pf: &str,
    pn: &str,
    pvr: &str,
    eapi: &str,
    repository: &str,
) -> PackageIdent {
    let revision = Version::parse(pvr).map(|v| v.revision()).unwrap_or(0);
    let pv = if revision > 0 {
        pvr.rsplit_once("-r")
            .map(|(base, _)| base.to_owned())
            .unwrap_or_else(|| pvr.to_owned())
    } else {
        pvr.to_owned()
    };
    PackageIdent {
        category: category.to_owned(),
        pf: pf.to_owned(),
        p: format!("{pn}-{pv}"),
        pn: pn.to_owned(),
        pv,
        pvr: pvr.to_owned(),
        pr: format!("r{revision}"),
        eapi: eapi.to_owned(),
        repository: repository.to_owned(),
    }
}

/// Split a shell-style command template into tokens.
fn tokenize(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

/// Obtain the repository index and the store directory backing it.
///
/// An existing greenfield store is loaded read-only without rebuilding: first the
/// system store (so a root-built cache is reused by any user), then the per-user
/// store. Only when none exists is the index built, into the system cache when
/// writable (root) or the per-user cache otherwise. The store refreshes on
/// `moraine --sync`, not on every invocation.
fn obtain_index(
    repos_conf: &Path,
    eroot: &Path,
    interner: &Arc<Interner>,
) -> Result<(RepoIndex, PathBuf)> {
    let system = eroot.join("var/cache/moraine/repos");
    let user = user_cache_base().map(|b| b.join("moraine/repos"));

    if let Some(index) = load_existing_index(repos_conf, &system, interner) {
        return Ok((index, system));
    }
    if let Some(user_dir) = &user
        && let Some(index) = load_existing_index(repos_conf, user_dir, interner)
    {
        return Ok((index, user_dir.clone()));
    }

    let build_dir = if is_writable(&system) {
        system
    } else {
        user.unwrap_or(system)
    };
    let index = build_index_with(repos_conf, &build_dir, Some(Arc::clone(interner)))
        .map_err(|e| index_error(e, &build_dir))?;
    Ok((index, build_dir))
}

/// Load a repository index from existing `.mrepo` store files without rebuilding.
/// Returns `None` when no repository has a store file yet.
fn load_existing_index(
    repos_conf: &Path,
    store_dir: &Path,
    interner: &Arc<Interner>,
) -> Option<RepoIndex> {
    let set = discover(repos_conf).ok()?;
    let mut repos = Vec::new();
    for cfg in set.ordered() {
        let path = store_dir.join(format!("{}.mrepo", cfg.name));
        // Skip a missing or unreadable store rather than abandoning the whole
        // read-only load (which would force an unnecessary rebuild).
        match LoadedStore::load_with(&path, Arc::clone(interner)) {
            Ok(store) => repos.push(RepoStore {
                name: cfg.name.clone(),
                store,
            }),
            Err(_) => continue,
        }
    }
    if repos.is_empty() {
        return None;
    }
    Some(RepoIndex::new(repos))
}

/// Whether a directory can be created and written to.
fn is_writable(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = dir.join(".moraine-write-test");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// The per-user cache base, from `XDG_CACHE_HOME` or `~/.cache`.
fn user_cache_base() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
}

/// Turn a repository-index build failure into an actionable diagnostic. A
/// permission failure on the store directory almost always means the command was
/// run without root, so it earns a specific hint.
fn index_error(error: moraine_repo::RepoError, store_dir: &Path) -> miette::Report {
    let denied = error
        .to_string()
        .to_lowercase()
        .contains("permission denied")
        || matches!(
            &error,
            moraine_repo::RepoError::Common(moraine_common::CommonError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::PermissionDenied
        );
    if denied {
        return miette!(
            help = "installing modifies the system, so run it as root (for example \
                    with `sudo`), or point `--root`/`--config-root` at a writable \
                    location for testing",
            "cannot write the repository cache at {}: permission denied",
            store_dir.display()
        );
    }
    miette!("could not build the repository index: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_pf_finds_version() {
        assert_eq!(
            split_pf("openssl-3.0.1-r1"),
            ("openssl".to_owned(), "3.0.1-r1".to_owned())
        );
        assert_eq!(split_pf("gtk+-2.0"), ("gtk+".to_owned(), "2.0".to_owned()));
    }

    #[test]
    fn package_ident_splits_revision() {
        let id = package_ident(
            "dev-libs",
            "openssl-3.0.1-r1",
            "openssl",
            "3.0.1-r1",
            "8",
            "gentoo",
        );
        assert_eq!(id.pv, "3.0.1");
        assert_eq!(id.pvr, "3.0.1-r1");
        assert_eq!(id.pr, "r1");
        assert_eq!(id.p, "openssl-3.0.1");
    }

    fn prefs(getbinpkg: bool, usepkg: bool) -> BinaryPrefs {
        BinaryPrefs {
            getbinpkg,
            usepkg,
            buildpkg: false,
            buildpkgonly: false,
        }
    }

    #[test]
    fn binary_choice_defaults_to_source() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            binary_choice("cat/pkg", "1", &prefs(false, false), dir.path(), None),
            (false, false)
        );
    }

    #[test]
    fn binary_choice_prefers_local_with_usepkg() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("cat")).unwrap();
        std::fs::write(dir.path().join("cat/pkg-1.gpkg"), b"x").unwrap();
        assert_eq!(
            binary_choice("cat/pkg", "1", &prefs(false, true), dir.path(), None),
            (true, false)
        );
    }

    #[test]
    fn binary_choice_without_binhost_match_builds_from_source() {
        let dir = tempfile::tempdir().unwrap();
        // getbinpkg with no local package and no binhost listing the cpv must
        // fall back to source, not blindly claim a binary package exists.
        assert_eq!(
            binary_choice("cat/pkg", "1", &prefs(true, true), dir.path(), None),
            (false, false)
        );
    }

    #[test]
    fn features_drive_binary_prefs() {
        let cli = Cli::parse_from_args(["cat/pkg"].map(String::from)).unwrap();
        let p = BinaryPrefs::from(&cli, &["getbinpkg".to_owned(), "buildpkg".to_owned()]);
        assert!(p.getbinpkg && p.usepkg && p.buildpkg);
    }

    #[test]
    fn planner_missing_entry_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc/portage")).unwrap();
        std::fs::write(
            dir.path().join("etc/portage/repos.conf"),
            format!(
                "[gentoo]\nlocation = {}\n",
                dir.path().join("repo").display()
            ),
        )
        .unwrap();
        let repo_set = discover(dir.path().join("etc/portage/repos.conf")).unwrap();
        let interner = Arc::new(Interner::new());
        let config = resolve_config(
            &Default::default(),
            &Default::default(),
            dir.path(),
            &[],
            Vec::new(),
            Vec::new(),
            &interner,
        );
        let ctx = ConfigContext {
            profile: Default::default(),
            vars: Default::default(),
            arch: String::new(),
            features: Vec::new(),
            config_protect: Vec::new(),
            config_protect_mask: Vec::new(),
            system: Vec::new(),
            selected: Vec::new(),
            profile_set: Vec::new(),
            world: Vec::new(),
            preserved_rebuild: Vec::new(),
        };
        let planner = CliPlanner {
            repo_set: &repo_set,
            store_dir: dir.path().join("empty-store"),
            config: &config,
            ctx: &ctx,
            eroot: dir.path().to_path_buf(),
            interner: Arc::clone(&interner),
            cache: RefCell::new(HashMap::new()),
            slot_bindings: HashMap::new(),
        };
        let task = InstallTask::merge("dev-libs/absent-1", "dev-libs/absent", "0");
        let err = planner.plan(&task).unwrap_err();
        assert!(matches!(err, InstallError::Realize { .. }));
    }
}
