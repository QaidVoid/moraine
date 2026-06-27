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
    Modifiers, RealSource, ResolveSource, Task, TaskKind as ResolveTaskKind, resolve_with,
    serialize,
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
    /// Only binary packages may be used; a package with no compatible binary is
    /// unsatisfiable rather than built from source (`--usepkgonly`).
    usepkgonly: bool,
    buildpkg: bool,
    buildpkgonly: bool,
    buildsyspkg: bool,
}

impl BinaryPrefs {
    fn from(cli: &Cli, features: &[String]) -> Self {
        let has = |name: &str| features.iter().any(|f| f == name);
        BinaryPrefs {
            // `getbinpkg` also implies considering binary packages, like emerge.
            getbinpkg: cli.getbinpkg || has("getbinpkg"),
            // `--usepkgonly` also considers binary packages.
            usepkg: cli.usepkg || cli.usepkgonly || cli.getbinpkg || has("getbinpkg"),
            usepkgonly: cli.usepkgonly,
            buildpkg: cli.buildpkg || has("buildpkg"),
            buildpkgonly: cli.buildpkgonly,
            // `buildsyspkg` emits binaries only for `@system` members.
            buildsyspkg: has("buildsyspkg"),
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
    let bin_candidates = binary_candidates(&pkgdir, binhost.as_ref(), &repo_index);
    let bin_target = if bin_candidates.is_empty() {
        None
    } else {
        Some(binary_target(&config, &ctx.vars, &bin_candidates, &vdb))
    };
    let bctx = BinaryContext {
        candidates: bin_candidates,
        target: bin_target,
    };

    // Resolve and serialize the merge order, timing the solve. The resolver
    // prefers a version with a compatible binary package.
    let source = RealSource::new(&repo_index, &vdb, &config)
        .with_binaries(bctx.candidates.clone(), bctx.target.clone());
    let atom_refs: Vec<&str> = request.atoms.iter().map(String::as_str).collect();
    let started = std::time::Instant::now();
    let modifiers = Modifiers {
        update: request.update,
        deep: request.deep,
        deep_depth: request.deep_depth,
        newuse: request.newuse,
        changed_use: cli.changed_use,
        changed_deps: cli.changed_deps,
        changed_slot: cli.changed_slot,
        autounmask: Default::default(),
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
        &bctx,
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

    // A change the policy keeps locked (a keyword or license change by default)
    // is a suggestion only. Refuse to build or merge the masked candidate and
    // present the required configuration change, mirroring emerge.
    if solution.autounmask.iter().any(|c| !c.auto_applied) {
        return Err(miette!(
            "the changes shown above are required to proceed. Apply them and re-run."
        ));
    }

    if cli.pretend {
        return Ok(());
    }
    if !crate::write::confirm(cli.ask) {
        println!("Operation cancelled.");
        return Ok(());
    }

    // Convert to orchestrator tasks, choosing source or binary per task. The
    // world-atom inputs mirror `create_world_atom`: the requesting argument's
    // repo and precision, the slotted `cp`s, and the system set.
    let world_inputs = WorldAtomInputs::compute(&qualified, &source, &installed, ctx, &interner);
    let tasks: Vec<InstallTask> = order
        .iter()
        .map(|task| {
            to_install_task(
                task,
                &world_inputs,
                &prefs,
                cli.oneshot,
                &pkgdir,
                binhost.as_ref(),
                &bctx,
            )
        })
        .collect();

    // Under `--usepkgonly` a package with no compatible binary is unsatisfiable
    // rather than built from source, matching emerge's `--usepkgonly`.
    if prefs.usepkgonly {
        let unsatisfiable = usepkgonly_unsatisfiable(&tasks);
        if !unsatisfiable.is_empty() {
            return Err(miette!(
                "--usepkgonly: no compatible binary package for {}",
                unsatisfiable.join(", ")
            ));
        }
    }

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
        buildsyspkg: prefs.buildsyspkg,
        system_cps: ctx.system.iter().map(|atom| cp_of_atom(atom)).collect(),
        pkgdir: pkgdir.clone(),
        binpkg_format: moraine_binpkg::BinpkgFormat::parse(
            ctx.vars.get("BINPKG_FORMAT").unwrap_or("gpkg"),
        ),
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
    let signature_policy = crate::config::signature_policy(&ctx.features);
    // The build engine answers build-time has_version/best_version from the
    // already-loaded installed store through this backend.
    let version_query = moraine_install::StoreVersionQuery::new(&vdb);
    let runner = CombinedRunner {
        source: SourceRunner::new(planner, &command_runner, options, &version_query),
        binpkg: BinpkgRunner::new(binpkg_source, stage)
            .with_signature(signature_policy, signature_config(&ctx.vars)),
    };
    let applier = EngineApplier::new(merge_context(ctx, &wr, cli.noconfmem));
    let engine = TransactionEngine::new(&runner, &applier, &wr.state_dir);
    let report = engine
        .run(&Transaction::new(tasks))
        .map_err(|e| miette!("install failed: {e}"))?;
    // Dispatch the build-time elog through the configured modules.
    crate::elog::dispatch(
        &report.elog,
        ctx.vars.get("PORTAGE_ELOG_CLASSES"),
        ctx.vars.get("PORTAGE_ELOG_SYSTEM"),
        ctx.vars.get("PORTAGE_ELOG_COMMAND"),
        &wr.eroot,
    );
    println!("Installation complete.");
    // Surface relevant unread news for every repository after the install.
    crate::news_state::display_after_action(ctx, &wr.vdb_dir, &wr.eroot, &repo_set);
    Ok(())
}

/// The signature verification key configuration, derived from the gpg command
/// and optional keyring named in configuration, used when reading a binary
/// package at install time.
fn signature_config(
    vars: &moraine_config::makeconf::VarMap,
) -> Option<moraine_binpkg::SignatureConfig> {
    let mut config = moraine_binpkg::SignatureConfig::default();
    if let Some(gpg) = vars.get("BINPKG_GPG_VERIFY_GPG").filter(|s| !s.is_empty()) {
        config.gpg_command = gpg.to_owned();
    }
    if let Some(home) = vars
        .get("BINPKG_GPG_VERIFY_GPG_HOME")
        .filter(|s| !s.is_empty())
    {
        config.extra_args.push("--homedir".to_owned());
        config.extra_args.push(home.to_owned());
    }
    Some(config)
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

        // The per-package environment overlay and bashrc selection are keyed by
        // the package, so build its reference for the config lookup.
        let version = Version::parse(&entry.version).map_err(|_| InstallError::Realize {
            cpv: task.cpv.clone(),
            reason: format!("invalid version `{}`", entry.version),
        })?;
        let pref = moraine_atom::PackageRef {
            category: self.interner.intern(&entry.category),
            package: self.interner.intern(&entry.package),
            version: &version,
            slot: Some(self.interner.intern(&entry.slot)),
            subslot: entry.subslot.as_deref().map(|s| self.interner.intern(s)),
            repo: Some(self.interner.intern(&entry.repository)),
        };

        // The eclass search path is per-repository, so it is set on the config
        // for this package rather than in the shared config_env.
        let mut config = self.config_env(&pref);
        config.eclass_locations = self.eclass_locations(&entry.repository);

        Ok(BuildRequest {
            package,
            config,
            use_flags,
            // Left empty so the strict use()/in_iuse check stays disabled until
            // the full IUSE_EFFECTIVE (with implicit/forced/masked flags) is
            // computed here; an incomplete set would make use() die spuriously.
            iuse_effective: Vec::new(),
            fetch: self.fetch_config(&entry.repository),
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
        let restrict_test = entry.restrict.iter().any(|r| r == "test");
        self.config
            .effective_use(&pref, &entry.iuse, false, restrict_test)
            .enabled
            .into_iter()
            .collect()
    }

    /// The build-environment configuration from `make.conf`, with the per-package
    /// `package.env` overlay applied: for an incremental variable the overlay
    /// appends to the global value, for any other variable it replaces it, and
    /// `FEATURES` is recomputed when the overlay changes it. The package's
    /// bashrc files are carried through for the phase driver to source.
    fn config_env(&self, pref: &moraine_atom::PackageRef) -> ConfigEnv {
        // Apply the per-package overlay onto a copy of the global vars, using the
        // make.conf merge semantics (append for incrementals, replace otherwise).
        let overlay = self.config.package_env_overlay(pref);
        let mut merged = self.ctx.vars.clone();
        let mut overlay_changes_features = false;
        for (key, value) in &overlay {
            if key == "FEATURES" {
                overlay_changes_features = true;
            }
            merged.merge_var(key, value);
        }

        let mut vars = std::collections::BTreeMap::new();
        for (key, value) in merged.iter() {
            vars.insert(key.clone(), value.clone());
        }
        let mirrors = merged
            .get("GENTOO_MIRRORS")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        // When the overlay changed FEATURES, recompute the effective features
        // from the merged value; otherwise reuse the global features.
        let features = if overlay_changes_features {
            merged
                .get("FEATURES")
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        } else {
            self.ctx.features.clone()
        };
        let bashrc_files = self
            .config
            .bashrc_files(pref)
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let root = self.eroot.to_string_lossy().into_owned();
        ConfigEnv {
            vars,
            features,
            mirrors,
            root: root.clone(),
            sysroot: root,
            eprefix: String::new(),
            config_root: self.eroot.to_string_lossy().into_owned(),
            eclass_locations: Vec::new(),
            bashrc_files,
        }
    }

    /// The eclass search locations for a repository, in the order `inherit`
    /// walks them (closest repository first), as exported strings.
    fn eclass_locations(&self, repo: &str) -> Vec<String> {
        eclass_locations(self.repo_set, repo)
    }

    /// The fetch configuration from `make.conf`, with the required-hash policy
    /// taken from the distfiles' owning repository.
    fn fetch_config(&self, owning_repo: &str) -> FetchConfig {
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
        // Protocol-specific FETCHCOMMAND_<PROTO>/RESUMECOMMAND_<PROTO> templates.
        let mut fetchcommand_proto = std::collections::BTreeMap::new();
        let mut resumecommand_proto = std::collections::BTreeMap::new();
        for (key, value) in self.ctx.vars.iter() {
            if let Some(proto) = key.strip_prefix("FETCHCOMMAND_") {
                fetchcommand_proto.insert(proto.to_ascii_lowercase(), tokenize(value));
            } else if let Some(proto) = key.strip_prefix("RESUMECOMMAND_") {
                resumecommand_proto.insert(proto.to_ascii_lowercase(), tokenize(value));
            }
        }
        let ro_distdirs = self
            .ctx
            .vars
            .get("PORTAGE_RO_DISTDIRS")
            .unwrap_or_default()
            .split_whitespace()
            .map(PathBuf::from)
            .collect();
        let checksum_try_mirrors = self
            .ctx
            .vars
            .get("PORTAGE_FETCH_CHECKSUM_TRY_MIRRORS")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(5);

        FetchConfig {
            distdir,
            fetchcommand,
            resumecommand,
            mirrors,
            thirdparty: crate::config::thirdparty_mirrors(self.repo_set),
            resume_min_size: 350_000,
            max_attempts: 3,
            required_hashes: required_manifest_hashes(self.repo_set, owning_repo),
            fetchcommand_proto,
            resumecommand_proto,
            ssh_opts: self
                .ctx
                .vars
                .get("PORTAGE_SSH_OPTS")
                .unwrap_or_default()
                .to_string(),
            checksum_try_mirrors,
            distlocks: self.ctx.features.iter().any(|f| f == "distlocks"),
            ro_distdirs,
            custom_mirrors: crate::config::custom_mirrors(&self.eroot),
            force_mirror: self.ctx.features.iter().any(|f| f == "force-mirror"),
        }
    }
}

/// The `manifest-required-hashes` of the distfiles' owning repository,
/// defaulting to `{BLAKE2B, SHA512}` when the repository is unknown or declares
/// none. Each distfile is verified against its own repository's policy rather
/// than a global union across repositories, matching Portage's per-repo
/// `required_hashes`.
fn required_manifest_hashes(
    repo_set: &moraine_repo::RepoSet,
    owning_repo: &str,
) -> std::collections::BTreeSet<String> {
    let owned: std::collections::BTreeSet<String> = repo_set
        .get(owning_repo)
        .map(|r| r.manifest_required_hashes.iter().cloned().collect())
        .unwrap_or_default();
    if owned.is_empty() {
        ["BLAKE2B", "SHA512"]
            .into_iter()
            .map(String::from)
            .collect()
    } else {
        owned
    }
}

/// Convert a serialized task into an orchestrator task.
/// The cpvs of merge tasks that resolved to a source build, which are the
/// packages with no compatible binary. Under `--usepkgonly` these are reported
/// unsatisfiable instead of being built.
fn usepkgonly_unsatisfiable(tasks: &[InstallTask]) -> Vec<String> {
    tasks
        .iter()
        .filter(|t| {
            matches!(t.kind, moraine_install::TaskKind::Merge)
                && matches!(t.source, SourceKind::Source)
        })
        .map(|t| t.cpv.clone())
        .collect()
}

fn to_install_task(
    task: &Task,
    world: &WorldAtomInputs,
    prefs: &BinaryPrefs,
    oneshot: bool,
    pkgdir: &Path,
    binhost: Option<&crate::binhost::IndexedBinhost>,
    bctx: &BinaryContext,
) -> InstallTask {
    let cpv = format!("{}-{}", task.cp, task.version);
    let kind = match task.kind {
        ResolveTaskKind::Uninstall => moraine_install::TaskKind::Uninstall,
        ResolveTaskKind::Merge => moraine_install::TaskKind::Merge,
    };
    // A binary is installed only when one is available and compatible; an
    // incompatible binary falls back to a source build.
    let (binary, _) = binary_choice(&task.cp, &task.version, prefs, pkgdir, binhost);
    let binary = binary && bctx.compatible(&cpv);
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
        world_atom: world.world_atom(task, oneshot),
        replaces: None,
    }
}

/// What a requesting command-line argument named beyond its `category/package`,
/// used to compute the world atom in the shape of `create_world_atom`.
struct ArgDetail {
    /// The repository qualifier the argument carried (`::repo`), if any.
    repo: Option<String>,
    /// Whether the argument identifies exactly one slot: it carried a version
    /// operator, version, or slot and matches a single slot among the available
    /// versions, mirroring `create_world_atom`'s `matched_slots` check.
    single_slot: bool,
}

/// The inputs needed to compute a world atom for a resolved task, mirroring
/// `lib/_emerge/create_world_atom.py`: the requesting argument detail keyed by
/// `cp`, the slotted `cp`s, and the `category/package` heads of the system set.
struct WorldAtomInputs {
    args: HashMap<String, ArgDetail>,
    slotted: HashSet<String>,
    system: BTreeSet<String>,
}

impl WorldAtomInputs {
    /// Build the world-atom inputs from the qualified targets, the resolver
    /// source (for repository slots), the installed-version map (for installed
    /// slots), and the configuration.
    fn compute<S: ResolveSource>(
        qualified: &[String],
        source: &S,
        installed: &HashMap<String, Vec<(String, String)>>,
        ctx: &ConfigContext,
        interner: &Interner,
    ) -> WorldAtomInputs {
        let mut args: HashMap<String, ArgDetail> = HashMap::new();
        for target in qualified {
            if target.starts_with('@') {
                continue;
            }
            let cp = cp_of_atom(target);
            let detail = match moraine_atom::Atom::parse(target, moraine_eapi::PERMISSIVE, interner)
            {
                Ok(atom) => {
                    // A bare `cat/pkg` (no version, no slot) never records a slot
                    // atom, matching `arg_atom.without_repo != cp`.
                    let bare = atom.version().is_none() && atom.slot().is_none();
                    ArgDetail {
                        repo: atom
                            .repo()
                            .and_then(|r| interner.resolve(r))
                            .map(|s| s.to_string()),
                        single_slot: !bare
                            && arg_single_slot(&atom, &cp, source, installed, interner),
                    }
                }
                Err(_) => ArgDetail {
                    repo: None,
                    single_slot: false,
                },
            };
            args.insert(cp, detail);
        }
        let slotted: HashSet<String> = args
            .keys()
            .filter(|cp| is_slotted(cp, source, installed))
            .cloned()
            .collect();
        let system: BTreeSet<String> = ctx.system.iter().map(|a| cp_of_atom(a)).collect();
        WorldAtomInputs {
            args,
            slotted,
            system,
        }
    }

    /// The world atom to record for a resolved task, or `None` when it should not
    /// join world. Mirrors `create_world_atom`: a dependency-only task or a
    /// `--oneshot` target never joins; an unslotted system member (other than a
    /// `virtual/*`) is omitted; a slotted package whose argument identifies a
    /// single slot records `cp:slot`; the `::repo` qualifier is preserved.
    fn world_atom(&self, task: &Task, oneshot: bool) -> Option<String> {
        if oneshot {
            return None;
        }
        let arg = self.args.get(&task.cp)?;
        let slotted = self.slotted.contains(&task.cp);
        if !slotted
            && arg.repo.is_none()
            && self.system.contains(&task.cp)
            && !task.cp.starts_with("virtual/")
        {
            return None;
        }
        let mut atom = if slotted && arg.single_slot {
            format!("{}:{}", task.cp, task.slot)
        } else {
            task.cp.clone()
        };
        if let Some(repo) = &arg.repo {
            atom.push_str("::");
            atom.push_str(repo);
        }
        Some(atom)
    }
}

/// Whether `cp` is slotted: the available slots across the repository candidates
/// and the installed versions number more than one, or the single slot is not
/// `0`. Mirrors the `slotted` computation in `create_world_atom`.
fn is_slotted<S: ResolveSource>(
    cp: &str,
    source: &S,
    installed: &HashMap<String, Vec<(String, String)>>,
) -> bool {
    let mut slots: BTreeSet<String> = source.versions_of(cp).into_iter().map(|m| m.slot).collect();
    if let Some(versions) = installed.get(cp) {
        slots.extend(versions.iter().map(|(_, slot)| slot.clone()));
    }
    slots.len() > 1 || (slots.len() == 1 && !slots.contains("0"))
}

/// Whether `atom` matches exactly one slot among the repository candidates and
/// installed versions of `cp`, mirroring `create_world_atom`'s `matched_slots`
/// check. The candidate repository is treated as the atom's own, so a
/// `::repo`-qualified argument still matches by version and slot.
fn arg_single_slot<S: ResolveSource>(
    atom: &moraine_atom::Atom,
    cp: &str,
    source: &S,
    installed: &HashMap<String, Vec<(String, String)>>,
    interner: &Interner,
) -> bool {
    let Some((category, package)) = cp.split_once('/') else {
        return false;
    };
    let cat = interner.intern(category);
    let pkg = interner.intern(package);
    let mut slots: BTreeSet<String> = BTreeSet::new();
    for meta in source.versions_of(cp) {
        let pref = moraine_atom::PackageRef {
            category: cat,
            package: pkg,
            version: &meta.version,
            slot: Some(interner.intern(&meta.slot)),
            subslot: meta.subslot.as_deref().map(|s| interner.intern(s)),
            repo: atom.repo(),
        };
        if atom.matches(&pref) {
            slots.insert(meta.slot);
        }
    }
    if let Some(versions) = installed.get(cp) {
        for (version, slot) in versions {
            let Ok(parsed) = Version::parse(version) else {
                continue;
            };
            let pref = moraine_atom::PackageRef {
                category: cat,
                package: pkg,
                version: &parsed,
                slot: Some(interner.intern(slot)),
                subslot: None,
                repo: atom.repo(),
            };
            if atom.matches(&pref) {
                slots.insert(slot.clone());
            }
        }
    }
    slots.len() == 1
}

/// The binary candidates available for selection plus the target configuration
/// they are checked against, so a binary is offered only when its recorded USE,
/// CHOST, and soname REQUIRES are compatible with the resolved configuration.
struct BinaryContext {
    /// Binary candidates keyed by `cpv` (`category/package-version`), from the
    /// binhost index plus local `.gpkg.tar` packages.
    candidates: HashMap<String, moraine_binpkg::BinaryCandidate>,
    /// The target configuration, or `None` when no compatibility gating applies.
    target: Option<moraine_binpkg::TargetConfig>,
}

impl BinaryContext {
    /// Whether the binary for `cpv` is compatible with the target.
    ///
    /// A candidate absent from the map (no recorded metadata) or failing the
    /// compatibility check is not compatible, so the ebuild candidate is used.
    fn compatible(&self, cpv: &str) -> bool {
        let Some(candidate) = self.candidates.get(cpv) else {
            return false;
        };
        match &self.target {
            None => true,
            Some(target) => {
                moraine_binpkg::check_compatibility(candidate, target)
                    == moraine_binpkg::Verdict::Accept
            }
        }
    }
}

/// Build the binary candidates available for selection: every binhost index
/// stanza (newest build per cpv) plus every local `.gpkg.tar` package under
/// `pkgdir`, including the multi-instance `<cp>/<pf>-<buildid>.gpkg.tar` layout.
fn binary_candidates(
    pkgdir: &Path,
    binhost: Option<&crate::binhost::IndexedBinhost>,
    repo_index: &RepoIndex,
) -> HashMap<String, moraine_binpkg::BinaryCandidate> {
    let mut map: HashMap<String, moraine_binpkg::BinaryCandidate> = HashMap::new();
    if let Some(bh) = binhost {
        for (cpv, metadata) in bh.candidate_metadata() {
            map.insert(
                cpv.clone(),
                candidate_from(&cpv, metadata.clone(), repo_index),
            );
        }
    }
    // Local packages override binhost stanzas: the on-disk metadata is read
    // directly from the container.
    for (cpv, path) in local_gpkg_files(pkgdir) {
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(pkg) = moraine_binpkg::read_package(&bytes, None)
        {
            map.insert(cpv.clone(), candidate_from(&cpv, pkg.metadata, repo_index));
        }
    }
    map
}

/// The IUSE of the ebuild matching `cpv` in the repo index, with the `+`/`-`
/// default prefix stripped, empty when no ebuild matches. Threaded onto a
/// [`moraine_binpkg::BinaryCandidate`] so the USE check can reject a binary
/// built against a different IUSE set than the tree's current ebuild.
fn ebuild_iuse(repo_index: &RepoIndex, cpv: &str) -> BTreeSet<String> {
    let candidates = repo_index.match_atom_str(&format!("={cpv}"));
    let Some(candidate) = candidates.first() else {
        return BTreeSet::new();
    };
    let Some(rs) = repo_index.repos().get(candidate.repo_order) else {
        return BTreeSet::new();
    };
    let interner = rs.store.interner();
    candidate
        .entry
        .iuse
        .iter()
        .filter_map(|s| {
            interner
                .resolve(*s)
                .map(|x| x.trim_start_matches(['+', '-']).to_string())
        })
        .collect()
}

/// Every local `.gpkg.tar` package under `pkgdir` as `(cpv, path)` pairs,
/// covering the single-instance `<category>/<pf>.gpkg.tar` and the multi-instance
/// `<cp>/<pf>-<buildid>.gpkg.tar` subdirectory layout.
fn local_gpkg_files(pkgdir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(categories) = std::fs::read_dir(pkgdir) else {
        return out;
    };
    for cat in categories.flatten() {
        if !cat.path().is_dir() {
            continue;
        }
        let category = cat.file_name().to_string_lossy().into_owned();
        let Ok(entries) = std::fs::read_dir(cat.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() {
                // Multi-instance `<category>/<package>/<pf>-<buildid>.gpkg.tar`.
                if let Ok(files) = std::fs::read_dir(entry.path()) {
                    for f in files.flatten() {
                        let fname = f.file_name().to_string_lossy().into_owned();
                        let Some(stem) = fname.strip_suffix(".gpkg.tar") else {
                            continue;
                        };
                        let pf = strip_build_id(stem);
                        out.push((format!("{category}/{pf}"), f.path()));
                    }
                }
            } else if let Some(pf) = name.strip_suffix(".gpkg.tar") {
                // Single-instance `<category>/<pf>.gpkg.tar`.
                out.push((format!("{category}/{pf}"), entry.path()));
            }
        }
    }
    out
}

/// Strip a trailing `-<buildid>` (numeric) from a multi-instance filename stem,
/// recovering the `<pf>`, matching Portage's `getname_build_id`.
fn strip_build_id(stem: &str) -> String {
    match stem.rsplit_once('-') {
        Some((pf, id)) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) => pf.to_owned(),
        _ => stem.to_owned(),
    }
}

/// Build a [`moraine_binpkg::BinaryCandidate`] from a cpv and recorded metadata,
/// populating `current_iuse` from the matching ebuild in `repo_index`.
fn candidate_from(
    cpv: &str,
    metadata: moraine_binpkg::MetadataMap,
    repo_index: &RepoIndex,
) -> moraine_binpkg::BinaryCandidate {
    let (category, pf) = split_cpv(cpv);
    let (pn, pvr) = split_pf(&pf);
    let cp = if category.is_empty() {
        pn
    } else {
        format!("{category}/{pn}")
    };
    let version = Version::parse(&pvr).unwrap_or_else(|_| Version::parse("0").unwrap());
    let current_iuse = ebuild_iuse(repo_index, cpv);
    moraine_binpkg::BinaryCandidate {
        cp,
        version,
        metadata,
        current_iuse,
    }
}

/// Build the global target configuration binary candidates are checked against:
/// the target `CHOST`, the globally selected/forced/masked USE, and the sonames
/// provided by the installed store and the binary candidates themselves.
fn binary_target(
    config: &ResolvedConfig,
    vars: &moraine_config::makeconf::VarMap,
    candidates: &HashMap<String, moraine_binpkg::BinaryCandidate>,
    vdb: &Store,
) -> moraine_binpkg::TargetConfig {
    let chost = vars.get("CHOST").unwrap_or_default().to_owned();

    // The globally selected USE, computed against a neutral package so only the
    // global and profile settings apply. `forced` from `EffectiveUse` is the
    // union of use.force and use.mask; a forced flag is enabled, a masked flag is
    // not, so the two are separated by membership in `enabled`.
    let interner = moraine_common::Interner::new();
    let dummy = Version::parse("0").unwrap();
    let pref = moraine_atom::PackageRef {
        category: interner.intern("null"),
        package: interner.intern("null"),
        version: &dummy,
        slot: Some(interner.intern("0")),
        subslot: None,
        repo: None,
    };
    let eu = config.effective_use(&pref, &[], false, false);
    let forced_use: BTreeSet<String> = eu.forced.intersection(&eu.enabled).cloned().collect();
    let masked_use: BTreeSet<String> = eu.forced.difference(&eu.enabled).cloned().collect();
    let selected_use = eu.enabled;

    let mut available_sonames: BTreeSet<(String, String)> = BTreeSet::new();
    let vdb_interner = vdb.interner();
    for record in vdb.records() {
        for e in &record.provides.entries {
            if let (Some(bucket), Some(soname)) = (
                vdb_interner.resolve(e.bucket),
                vdb_interner.resolve(e.soname),
            ) {
                available_sonames.insert((bucket.to_string(), soname.to_string()));
            }
        }
    }
    for candidate in candidates.values() {
        if let Some(provides) = candidate.metadata.get_str("PROVIDES") {
            for pair in moraine_binpkg::resolution::parse_sonames(&provides) {
                available_sonames.insert(pair);
            }
        }
    }

    moraine_binpkg::TargetConfig {
        chost,
        selected_use,
        forced_use,
        masked_use,
        available_sonames,
    }
}

fn binary_choice(
    cp: &str,
    version: &str,
    prefs: &BinaryPrefs,
    pkgdir: &Path,
    binhost: Option<&crate::binhost::IndexedBinhost>,
) -> (bool, bool) {
    let cpv = format!("{cp}-{version}");
    let local = moraine_install::locate_local_gpkg(pkgdir, cp, &cpv).is_some();
    if (prefs.usepkg || prefs.getbinpkg) && local {
        // A local binary package is present.
        (true, false)
    } else if prefs.getbinpkg && binhost.is_some_and(|bh| bh.contains(&cpv)) {
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
    bctx: &BinaryContext,
) {
    let mut cache: HashMap<String, Arc<Vec<StoredEntry>>> = HashMap::new();
    for entry in &mut plan.entries {
        if entry.operation == Operation::Uninstall {
            continue;
        }
        let cpv = format!("{}-{}", entry.cp, entry.version);
        let (mut binary, mut fetched) =
            binary_choice(&entry.cp, &entry.version, prefs, pkgdir, binhost);
        // An incompatible binary is not offered; the source build is shown.
        if binary && !bctx.compatible(&cpv) {
            binary = false;
            fetched = false;
        }
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
            let restrict_test = stored.restrict.iter().any(|r| r == "test");
            let forced = config
                .effective_use(&pref, &stored.iuse, false, restrict_test)
                .forced;
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

/// The on-disk size of a local binary package, if present, covering the
/// single-instance and multi-instance `.gpkg.tar` layouts.
fn binary_size(pkgdir: &Path, cpv: &str) -> Option<u64> {
    let (category, pf) = split_cpv(cpv);
    let cp = format!("{category}/{}", split_pf(&pf).0);
    let path = moraine_install::locate_local_gpkg(pkgdir, &cp, cpv)?;
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
    let restrict_test = stored.restrict.iter().any(|r| r == "test");
    let use_flags: HashSet<String> = config
        .effective_use(&pref, &stored.iuse, false, restrict_test)
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

/// Compute the `@preserved-rebuild` set: installed packages requiring a soname
/// kept alive only by a preserved library, minus the preserved libraries' own
/// owners. Returns empty when the registry has no preserved libraries.
fn compute_preserved_rebuild(vdb: &Store, state_dir: &Path) -> Vec<String> {
    let registry = moraine_merge::PreservedLibs::load(&state_dir.join("preserved-libs"))
        .unwrap_or_else(|_| moraine_merge::PreservedLibs::new());
    if registry.is_empty() {
        return Vec::new();
    }
    let interner = vdb.interner();
    // A soname is preserved-only when no non-preserved installed package provides
    // it in the same bucket, mirroring `findConsumers(greedy=False)`. The
    // preserved library itself is recorded under the new owner but never appears
    // in any record's PROVIDES, so a bucketed provides match is a real provider.
    let preserved_sonames: BTreeSet<(String, String)> = registry
        .entries()
        .iter()
        .map(|e| (e.bucket.clone(), e.soname.clone()))
        .filter(|(bucket, soname)| {
            let bucket_sym = interner.intern(bucket);
            let soname_sym = interner.intern(soname);
            !vdb.records()
                .iter()
                .any(|r| r.provides.provides_in(bucket_sym, soname_sym))
        })
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
    let consumers: Vec<(String, Vec<(String, String)>)> = vdb
        .records()
        .iter()
        .filter_map(|record| {
            let category = interner.resolve(record.category)?;
            let package = interner.resolve(record.package)?;
            let sonames = vdb
                .required_sonames(record)
                .filter_map(|(bucket, soname)| {
                    Some((
                        interner.resolve(bucket)?.to_string(),
                        interner.resolve(soname)?.to_string(),
                    ))
                })
                .collect();
            Some((format!("{category}/{package}"), sonames))
        })
        .collect();
    moraine_config::sets::preserved_rebuild_set(&consumers, &preserved_sonames, &preserved_owners)
}

/// The eclass search locations for a repository, in the order `inherit` walks
/// them (closest repository first), as the repository-root strings the bash
/// `inherit` appends `/eclass/<name>.eclass` to. `eclass_search_path` returns
/// the `<repo>/eclass` directories, so each is mapped back to its repo root.
pub(crate) fn eclass_locations(repo_set: &moraine_repo::RepoSet, repo: &str) -> Vec<String> {
    repo_set
        .eclass_search_path(repo)
        .into_iter()
        .map(|eclass_dir| {
            eclass_dir
                .parent()
                .unwrap_or(&eclass_dir)
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// Split a `category/package-version` into `(category, pf)`.
pub(crate) fn split_cpv(cpv: &str) -> (String, String) {
    match cpv.split_once('/') {
        Some((c, pf)) => (c.to_owned(), pf.to_owned()),
        None => (String::new(), cpv.to_owned()),
    }
}

/// Split a `pf` (`pn-version`) into `(pn, pvr)` at the version boundary.
pub(crate) fn split_pf(pf: &str) -> (String, String) {
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
pub(crate) fn package_ident(
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
    fn world_atom_records_slot_repo_and_omits_system() {
        let task = |cp: &str, slot: &str| Task {
            kind: ResolveTaskKind::Merge,
            cp: cp.to_owned(),
            version: "1".to_owned(),
            slot: slot.to_owned(),
            use_enabled: Vec::new(),
        };
        let mut args = HashMap::new();
        // A slotted package whose argument identifies a single slot.
        args.insert(
            "sys-devel/gcc".to_owned(),
            ArgDetail {
                repo: None,
                single_slot: true,
            },
        );
        // A ::repo-qualified argument.
        args.insert(
            "dev-libs/foo".to_owned(),
            ArgDetail {
                repo: Some("myrepo".to_owned()),
                single_slot: false,
            },
        );
        // An unslotted system member.
        args.insert(
            "sys-apps/sed".to_owned(),
            ArgDetail {
                repo: None,
                single_slot: false,
            },
        );
        let inputs = WorldAtomInputs {
            args,
            slotted: ["sys-devel/gcc".to_owned()].into_iter().collect(),
            system: ["sys-apps/sed".to_owned()].into_iter().collect(),
        };

        // A slotted package with a precise argument records cp:slot.
        assert_eq!(
            inputs.world_atom(&task("sys-devel/gcc", "13"), false),
            Some("sys-devel/gcc:13".to_owned())
        );
        // A ::repo argument records the repo qualifier.
        assert_eq!(
            inputs.world_atom(&task("dev-libs/foo", "0"), false),
            Some("dev-libs/foo::myrepo".to_owned())
        );
        // An unslotted system member is not recorded.
        assert_eq!(inputs.world_atom(&task("sys-apps/sed", "0"), false), None);
        // A dependency that was not requested is not recorded.
        assert_eq!(inputs.world_atom(&task("dev-libs/dep", "0"), false), None);
        // A --oneshot target is not recorded.
        assert_eq!(inputs.world_atom(&task("sys-devel/gcc", "13"), true), None);
    }

    #[test]
    fn split_pf_finds_version() {
        assert_eq!(
            split_pf("openssl-3.0.1-r1"),
            ("openssl".to_owned(), "3.0.1-r1".to_owned())
        );
        assert_eq!(split_pf("gtk+-2.0"), ("gtk+".to_owned(), "2.0".to_owned()));
    }

    #[test]
    fn usepkgonly_reports_source_merges_unsatisfiable() {
        let task = |cpv: &str, kind, source| InstallTask {
            cpv: cpv.to_owned(),
            cp: cpv
                .rsplit_once('-')
                .map(|(c, _)| c)
                .unwrap_or(cpv)
                .to_owned(),
            slot: "0".to_owned(),
            kind,
            source,
            world_atom: None,
            replaces: None,
        };
        let tasks = vec![
            task(
                "cat/a-1",
                moraine_install::TaskKind::Merge,
                SourceKind::Binary,
            ),
            task(
                "cat/b-2",
                moraine_install::TaskKind::Merge,
                SourceKind::Source,
            ),
            task(
                "cat/c-3",
                moraine_install::TaskKind::Uninstall,
                SourceKind::Source,
            ),
        ];
        // Only the source-build merge (no compatible binary) is unsatisfiable; a
        // binary merge and an uninstall are not.
        assert_eq!(usepkgonly_unsatisfiable(&tasks), vec!["cat/b-2".to_owned()]);
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
            usepkgonly: false,
            buildpkg: false,
            buildpkgonly: false,
            buildsyspkg: false,
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
        std::fs::write(dir.path().join("cat/pkg-1.gpkg.tar"), b"x").unwrap();
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
            set_search_dirs: Vec::new(),
            vdb_dir: std::path::PathBuf::from("/var/db/pkg"),
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

    #[test]
    fn config_env_applies_package_env_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let portage = dir.path().join("etc/portage");
        std::fs::create_dir_all(portage.join("env")).unwrap();
        std::fs::write(
            portage.join("repos.conf"),
            format!(
                "[gentoo]\nlocation = {}\n",
                dir.path().join("repo").display()
            ),
        )
        .unwrap();
        // package.env sets a per-package CFLAGS (replace) and FEATURES (append).
        std::fs::write(
            portage.join("package.env"),
            "dev-libs/foo lowopt.conf splitfeat.conf\n",
        )
        .unwrap();
        std::fs::write(portage.join("env/lowopt.conf"), "CFLAGS=\"-O1\"\n").unwrap();
        std::fs::write(
            portage.join("env/splitfeat.conf"),
            "FEATURES=\"splitdebug\"\n",
        )
        .unwrap();

        let repo_set = discover(portage.join("repos.conf")).unwrap();
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
        let mut vars = moraine_config::makeconf::VarMap::new();
        vars.set("CFLAGS".to_owned(), "-O2".to_owned());
        vars.set("FEATURES".to_owned(), "sandbox".to_owned());
        let ctx = ConfigContext {
            profile: Default::default(),
            vars,
            arch: String::new(),
            features: vec!["sandbox".to_owned()],
            config_protect: Vec::new(),
            config_protect_mask: Vec::new(),
            system: Vec::new(),
            selected: Vec::new(),
            profile_set: Vec::new(),
            world: Vec::new(),
            preserved_rebuild: Vec::new(),
            set_search_dirs: Vec::new(),
            vdb_dir: std::path::PathBuf::from("/var/db/pkg"),
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
        let version = Version::parse("1.0").unwrap();
        let pref = moraine_atom::PackageRef {
            category: interner.intern("dev-libs"),
            package: interner.intern("foo"),
            version: &version,
            slot: Some(interner.intern("0")),
            subslot: None,
            repo: Some(interner.intern("gentoo")),
        };
        let cfg = planner.config_env(&pref);
        // CFLAGS is non-incremental, so the overlay replaces the global value.
        assert_eq!(cfg.vars.get("CFLAGS").map(String::as_str), Some("-O1"));
        // FEATURES is incremental, so the overlay appends and is recomputed.
        assert!(cfg.features.iter().any(|f| f == "splitdebug"));
        assert!(cfg.features.iter().any(|f| f == "sandbox"));

        // A non-matching package keeps the global environment unchanged.
        let other = moraine_atom::PackageRef {
            package: interner.intern("bar"),
            ..pref
        };
        let cfg = planner.config_env(&other);
        assert_eq!(cfg.vars.get("CFLAGS").map(String::as_str), Some("-O2"));
    }
}
