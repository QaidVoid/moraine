//! Realizing tasks into merge operations.
//!
//! [`BinpkgRunner`] is a [`StepRunner`] that installs from binary packages: for
//! each task it locates the container (locally via [`LocalPkgdir`] or from a
//! binhost via [`BinhostSource`]), unpacks its image into a staging directory,
//! and builds the [`Operation`] the merge engine applies. The binary path is
//! self-contained because the container carries both the image and the metadata.
//!
//! [`SourceRunner`] is the from-source [`StepRunner`]: it asks a [`BuildPlanner`]
//! for a [`BuildRequest`], drives the build engine to produce an image, and
//! optionally emits a binary package. The planner is supplied by the caller,
//! which owns the repository metadata (including `SRC_URI`) and configuration.

use std::path::{Path, PathBuf};

use moraine_binpkg::greenfield::WriteOptions;
use moraine_binpkg::{MetadataMap, read_package_with_policy};
use moraine_build::{
    BuildOutcome, BuildRequest, CommandRunner, QueryRoot, VersionQuery, build_package,
};
use moraine_merge::state::PackageState;
use moraine_merge::{MergeOp, Operation};
use moraine_vdb::record::PackageRecord;
use moraine_vdb::store::Store;

use crate::error::{InstallError, Result};
use crate::quickpkg::package_image_dir;
use crate::step::StepRunner;
use crate::task::{InstallTask, Realized, SourceKind};

/// Locates a binary package container for a task.
pub trait BinpkgSource {
    /// Return the container bytes for `task`, or `None` when no compatible
    /// binary package is available.
    fn fetch(&self, task: &InstallTask) -> Result<Option<Vec<u8>>>;
}

/// A [`BinpkgSource`] backed by a local package directory laid out as the
/// single-instance `<pkgdir>/<category>/<pf>.gpkg.tar` or the multi-instance
/// `<pkgdir>/<cp>/<pf>-<buildid>.gpkg.tar`, matching Portage's
/// `getname_build_id` and `SUPPORTED_GPKG_EXTENSIONS`.
pub struct LocalPkgdir {
    /// The package directory root (`PKGDIR`).
    pub pkgdir: PathBuf,
}

impl BinpkgSource for LocalPkgdir {
    fn fetch(&self, task: &InstallTask) -> Result<Option<Vec<u8>>> {
        let Some(path) = locate_local_gpkg(&self.pkgdir, &task.cp, &task.cpv) else {
            return Ok(None);
        };
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(InstallError::io(path, e)),
        }
    }
}

/// Locate a local `.gpkg.tar` container for `cp`/`cpv` under `pkgdir`.
///
/// The single-instance `<category>/<pf>.gpkg.tar` is preferred; otherwise the
/// multi-instance `<cp>/<pf>-<buildid>.gpkg.tar` subdirectory is scanned and the
/// highest build id is returned.
pub fn locate_local_gpkg(pkgdir: &Path, cp: &str, cpv: &str) -> Option<PathBuf> {
    let (category, _) = cp.split_once('/').unwrap_or((cp, ""));
    let pf = cpv.rsplit('/').next().unwrap_or(cpv);
    let single = pkgdir.join(category).join(format!("{pf}.gpkg.tar"));
    if single.exists() {
        return Some(single);
    }
    // Multi-instance: `<pkgdir>/<cp>/<pf>-<buildid>.gpkg.tar`, newest build id.
    let dir = pkgdir.join(cp);
    let prefix = format!("{pf}-");
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".gpkg.tar") else {
            continue;
        };
        let Some(id) = stem.strip_prefix(&prefix) else {
            continue;
        };
        let Ok(build_id) = id.parse::<u64>() else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| build_id > *b) {
            best = Some((build_id, entry.path()));
        }
    }
    best.map(|(_, path)| path)
}

impl BinpkgSource for Box<dyn BinpkgSource> {
    fn fetch(&self, task: &InstallTask) -> Result<Option<Vec<u8>>> {
        (**self).fetch(task)
    }
}

/// A [`BinpkgSource`] that fetches containers from a binhost base URI, laid out
/// as `<base>/<category>/<pf>.gpkg.tar`, into a staging directory.
pub struct BinhostSource {
    /// The binhost base URI (`PORTAGE_BINHOST`).
    pub base_uri: String,
    /// The fetch command used to download containers.
    pub fetch: moraine_binpkg::fetch::FetchCommand,
    /// The directory downloaded containers are written to.
    pub stage_dir: PathBuf,
}

impl BinpkgSource for BinhostSource {
    fn fetch(&self, task: &InstallTask) -> Result<Option<Vec<u8>>> {
        if self.base_uri.is_empty() {
            return Ok(None);
        }
        let (category, _) = task.cp.split_once('/').unwrap_or((task.cp.as_str(), ""));
        let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
        let uri = format!(
            "{}/{}/{}.gpkg.tar",
            self.base_uri.trim_end_matches('/'),
            category,
            pf
        );
        std::fs::create_dir_all(&self.stage_dir)
            .map_err(|e| InstallError::io(&self.stage_dir, e))?;
        let dest = self.stage_dir.join(format!("{pf}.gpkg.tar"));
        // A fetch failure means the container is unavailable from the binhost,
        // not a hard error: the caller falls back or reports it per task.
        if self.fetch.run(&uri, &dest).is_err() {
            return Ok(None);
        }
        match std::fs::read(&dest) {
            Ok(bytes) if !bytes.is_empty() => Ok(Some(bytes)),
            _ => Ok(None),
        }
    }
}

/// A [`StepRunner`] that installs binary packages, staging each image under
/// `stage_dir`.
pub struct BinpkgRunner<S: BinpkgSource> {
    source: S,
    stage_dir: PathBuf,
    signature: Option<moraine_binpkg::SignatureConfig>,
    policy: moraine_binpkg::SignaturePolicy,
}

impl<S: BinpkgSource> BinpkgRunner<S> {
    /// Build a runner that stages unpacked images under `stage_dir`, applying the
    /// default `VerifyIfPresent` signature policy with no configured key.
    pub fn new(source: S, stage_dir: impl Into<PathBuf>) -> Self {
        BinpkgRunner {
            source,
            stage_dir: stage_dir.into(),
            signature: None,
            policy: moraine_binpkg::SignaturePolicy::default(),
        }
    }

    /// Set the signature `policy` and optional key `config` applied to each
    /// container read at install time.
    pub fn with_signature(
        mut self,
        policy: moraine_binpkg::SignaturePolicy,
        config: Option<moraine_binpkg::SignatureConfig>,
    ) -> Self {
        self.policy = policy;
        self.signature = config;
        self
    }
}

impl<S: BinpkgSource> StepRunner for BinpkgRunner<S> {
    fn realize(&self, task: &InstallTask) -> Result<Realized> {
        if task.source != SourceKind::Binary {
            return Err(InstallError::Realize {
                cpv: task.cpv.clone(),
                reason: "this is a binary-package runner; route source tasks to \
                         the source runner"
                    .to_owned(),
            });
        }
        let bytes = self
            .source
            .fetch(task)?
            .ok_or_else(|| InstallError::Realize {
                cpv: task.cpv.clone(),
                reason: "no compatible binary package found".to_owned(),
            })?;
        realize_binpkg(
            &bytes,
            task,
            &self.stage_dir,
            self.signature.as_ref(),
            self.policy,
        )
    }
}

/// Constructs a [`BuildRequest`] for a task from repository and configuration
/// data. Implemented by the caller, which owns the repo index and config; this
/// keeps the orchestrator decoupled from `moraine-repo` and `moraine-config`.
pub trait BuildPlanner {
    /// Plan the build for `task`, or return an error explaining why it cannot be
    /// built.
    fn plan(&self, task: &InstallTask) -> Result<BuildRequest>;
}

/// Binary-package emission options for the source build path.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Emit a binary package alongside merging (`--buildpkg`).
    pub buildpkg: bool,
    /// Emit a binary package and skip merging (`--buildpkgonly`).
    pub buildpkgonly: bool,
    /// `FEATURES=buildsyspkg`: emit a binary package for `@system` members even
    /// when global `buildpkg` is off.
    pub buildsyspkg: bool,
    /// The `category/package` heads of the `@system` set, used by `buildsyspkg`.
    pub system_cps: std::collections::BTreeSet<String>,
    /// The package directory to write binary packages into.
    pub pkgdir: PathBuf,
    /// The container write options.
    pub write_options: WriteOptions,
    /// The output container format from `BINPKG_FORMAT`.
    pub binpkg_format: moraine_binpkg::BinpkgFormat,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            buildpkg: false,
            buildpkgonly: false,
            buildsyspkg: false,
            system_cps: std::collections::BTreeSet::new(),
            pkgdir: PathBuf::from("/var/cache/binpkgs"),
            write_options: WriteOptions::default(),
            binpkg_format: moraine_binpkg::BinpkgFormat::default(),
        }
    }
}

/// A [`VersionQuery`] answered from the installed [`Store`], the backend the
/// build engine reaches for build-time `has_version`/`best_version` queries.
///
/// It mirrors the stock `QueryCommand`, which answers both queries from
/// `vardb.match(atom)`: `has_version` is true when the atom matches an installed
/// package, and `best_version` is the highest matching installed `cpv`. The
/// queried atom is parsed against the store's own interner so its symbols compare
/// equal to the recorded packages. A malformed atom surfaces as the invalid-atom
/// case so the bash wrapper reports it rather than treating it as a match miss.
///
/// The root selector is honored for the host root and falls back to the single
/// installed store for the cross roots, the seam where `ROOT`/`ESYSROOT`/`BROOT`
/// would select distinct stores once cross-root installs are modeled.
pub struct StoreVersionQuery<'a> {
    store: &'a Store,
}

impl<'a> StoreVersionQuery<'a> {
    /// Build the adapter over the loaded installed `store`.
    pub fn new(store: &'a Store) -> Self {
        StoreVersionQuery { store }
    }

    /// The installed records matching `atom`, parsed against the store interner.
    /// An unparseable atom matches nothing.
    fn matches(&self, atom: &str) -> Vec<&PackageRecord> {
        match moraine_atom::Atom::parse(atom, moraine_eapi::PERMISSIVE, self.store.interner()) {
            Ok(parsed) => self.store.match_atom(&parsed),
            Err(_) => Vec::new(),
        }
    }
}

impl VersionQuery for StoreVersionQuery<'_> {
    fn has_version(&self, _root: QueryRoot, atom: &str, _caller_use: &[String]) -> bool {
        !self.matches(atom).is_empty()
    }

    fn best_version(&self, _root: QueryRoot, atom: &str, _caller_use: &[String]) -> Option<String> {
        let interner = self.store.interner();
        self.matches(atom)
            .into_iter()
            .max_by(|a, b| a.version.cmp(&b.version))
            .map(|record| {
                let cat = interner
                    .resolve(record.category)
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                let pkg = interner
                    .resolve(record.package)
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                format!("{cat}/{pkg}-{}", record.version.as_str())
            })
    }

    fn invalid_atom(&self, atom: &str) -> bool {
        moraine_atom::Atom::parse(atom, moraine_eapi::PERMISSIVE, self.store.interner()).is_err()
    }
}

/// A [`StepRunner`] that builds from source through the build engine.
pub struct SourceRunner<'r, P: BuildPlanner, R: CommandRunner> {
    planner: P,
    runner: &'r R,
    options: BuildOptions,
    version_query: &'r dyn VersionQuery,
}

impl<'r, P: BuildPlanner, R: CommandRunner> SourceRunner<'r, P, R> {
    /// Build a source runner over `planner`, the external-command `runner`, the
    /// binary-package `options`, and the `version_query` backend the build engine
    /// answers `has_version`/`best_version` from.
    pub fn new(
        planner: P,
        runner: &'r R,
        options: BuildOptions,
        version_query: &'r dyn VersionQuery,
    ) -> Self {
        SourceRunner {
            planner,
            runner,
            options,
            version_query,
        }
    }
}

impl<P: BuildPlanner, R: CommandRunner> StepRunner for SourceRunner<'_, P, R> {
    fn realize(&self, task: &InstallTask) -> Result<Realized> {
        let request = self.planner.plan(task)?;
        let outcome =
            build_package(&request, self.runner, Some(self.version_query)).map_err(|e| {
                InstallError::Realize {
                    cpv: task.cpv.clone(),
                    reason: format!("build failed: {e}"),
                }
            })?;

        // Scan the staged image and read BUILD_TIME once, feeding both the binary
        // package metadata and the recorded installed state.
        let scan = moraine_build::scan_image_sonames(&outcome.image_dir);
        let build_time = read_line_u64(&outcome.build_info_dir.join("BUILD_TIME"));

        // `buildsyspkg` emits a binary package for an `@system` member even when
        // global `buildpkg` is off.
        let syspkg = self.options.buildsyspkg && self.options.system_cps.contains(&task.cp);
        if self.options.buildpkg || self.options.buildpkgonly || syspkg {
            let (category, _) = task.cp.split_once('/').unwrap_or((task.cp.as_str(), ""));
            let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
            let dir = self.options.pkgdir.join(category);
            std::fs::create_dir_all(&dir).map_err(|e| InstallError::io(&dir, e))?;
            let out = dir.join(format!("{pf}.{}", self.options.binpkg_format.extension()));
            let metadata = metadata_from_request(task, &request, &scan, build_time);
            package_image_dir(
                &task.cpv,
                &outcome.image_dir,
                &metadata,
                &out,
                &self.options.write_options,
                self.options.binpkg_format,
            )?;
        }

        if self.options.buildpkgonly {
            return Ok(Realized::PackagedOnly);
        }

        let state = state_from_request(task, &request, &outcome, scan, build_time);
        // Carry the build-time elog through the merge into the install report.
        let elog = outcome
            .report
            .elog
            .iter()
            .map(|m| moraine_merge::ElogRecord {
                level: elog_class(m.level).to_owned(),
                phase: m.phase.clone(),
                text: m.text.clone(),
            })
            .collect();
        let ebuild = read_build_info_ebuild(&outcome.build_info_dir);
        let op = MergeOp {
            image_dir: outcome.image_dir,
            state,
            replaces: task.replaces.clone(),
            world_atom: task.world_atom.clone(),
            elog,
            ebuild,
        };
        Ok(Realized::Apply(Operation::Merge(Box::new(op))))
    }
}

/// Read the ebuild copy the build placed in the build-info directory, so it can
/// be re-exported into the dbdir as `<PF>.ebuild`. Returns `None` when absent.
fn read_build_info_ebuild(build_info_dir: &Path) -> Option<Vec<u8>> {
    let read = std::fs::read_dir(build_info_dir).ok()?;
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "ebuild") {
            return std::fs::read(&path).ok();
        }
    }
    None
}

/// Map a build `ElogLevel` to its lowercase elog class name.
fn elog_class(level: moraine_build::ElogLevel) -> &'static str {
    use moraine_build::ElogLevel;
    match level {
        ElogLevel::Info => "info",
        ElogLevel::Log => "log",
        ElogLevel::Warn => "warn",
        ElogLevel::Error => "error",
        ElogLevel::Qa => "qa",
    }
}

/// Build the installed-state record from the task identity and the build
/// request's resolved package and USE set.
fn state_from_request(
    task: &InstallTask,
    request: &BuildRequest,
    outcome: &BuildOutcome,
    scan: moraine_build::SonameScan,
    build_time: Option<u64>,
) -> PackageState {
    let pkg = &request.package;
    let (category, package) = task.cp.split_once('/').unwrap_or((task.cp.as_str(), ""));
    let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
    let version = pf
        .strip_prefix(&format!("{package}-"))
        .unwrap_or(pf)
        .to_owned();

    let mut use_flags: Vec<String> = request.use_flags.iter().cloned().collect();
    use_flags.sort();

    // Bake each `:=` binding into the recorded `*DEPEND` against the provider it
    // linked against, so the stored dependency carries the bound slot/subslot
    // like Portage's `evaluate_slot_operator_equal_deps`.
    let interner = moraine_common::Interner::new();
    let mut depends = std::collections::BTreeMap::new();
    for key in ["DEPEND", "RDEPEND", "BDEPEND", "PDEPEND", "IDEPEND"] {
        if let Some(value) = pkg.reduced_meta.get(key)
            && !value.trim().is_empty()
        {
            let rewritten =
                moraine_merge::rewrite_slot_operators(value, &request.slot_bindings, &interner);
            depends.insert(key.to_owned(), rewritten);
        }
    }
    let meta = |key: &str| pkg.reduced_meta.get(key).cloned().unwrap_or_default();

    // The saved environment from build-info, alongside the scanned soname linkage
    // and BUILD_TIME passed in by the caller.
    let environment = std::fs::read(outcome.build_info_dir.join("environment.bz2")).ok();
    let to_sonames = |pairs: Vec<(String, String)>| -> Vec<moraine_merge::state::Soname> {
        pairs
            .into_iter()
            .map(|(bucket, soname)| moraine_merge::state::Soname { bucket, soname })
            .collect()
    };

    PackageState {
        cpv: task.cpv.clone(),
        category: category.to_owned(),
        package: package.to_owned(),
        version,
        eapi: pkg.ident.eapi.clone(),
        slot: pkg.slot.clone(),
        subslot: pkg.subslot.clone(),
        use_flags,
        iuse: pkg.iuse.clone(),
        depends,
        keywords: pkg.keywords.clone(),
        license: meta("LICENSE"),
        description: meta("DESCRIPTION"),
        homepage: meta("HOMEPAGE"),
        properties: meta("PROPERTIES"),
        restrict: pkg.restrict.join(" "),
        repository: Some(pkg.ident.repository.clone()),
        defined_phases: pkg.defined_phases.clone(),
        build_time,
        chost: request
            .config
            .vars
            .get("CHOST")
            .cloned()
            .unwrap_or_default(),
        provides: to_sonames(scan.provides),
        requires: to_sonames(scan.requires),
        environment,
        inherited: split_ws(&meta("INHERITED")),
        features: request
            .config
            .vars
            .get("FEATURES")
            .map(|s| split_ws(s))
            .unwrap_or_default(),
        size: Some(dir_size(&outcome.image_dir)),
        build_id: None,
        needed: render_needed_lines(&scan.needed_lines),
        toolchain: toolchain_from_vars(&request.config.vars),
    }
}

/// Read the recorded toolchain flag files from the resolved build configuration.
fn toolchain_from_vars(
    vars: &std::collections::BTreeMap<String, String>,
) -> moraine_vdb::record::Toolchain {
    let v = |key: &str| vars.get(key).cloned().unwrap_or_default();
    moraine_vdb::record::Toolchain {
        cbuild: v("CBUILD"),
        cc: v("CC"),
        cflags: v("CFLAGS"),
        cxx: v("CXX"),
        cxxflags: v("CXXFLAGS"),
        ctarget: v("CTARGET"),
        asflags: v("ASFLAGS"),
        ldflags: v("LDFLAGS"),
    }
}

/// Split a whitespace-separated string into owned tokens.
fn split_ws(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

/// Render scanned NEEDED lines into the `arch;path;soname;rpath;needed` text form.
fn render_needed_lines(lines: &[moraine_build::NeededLine]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            format!(
                "{};{};{};;{}",
                l.bucket,
                l.path,
                l.soname.clone().unwrap_or_default(),
                l.needed.join(",")
            )
        })
        .collect()
}

/// Sum the sizes of every regular file under `dir`, the installed `SIZE`.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in read.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// Read a single-line `u64` from `path`, returning `None` when absent or
/// unparseable.
fn read_line_u64(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Build a binary-package metadata map from the build request, for the
/// `--buildpkg` byproduct, including the scanned soname linkage and BUILD_TIME so
/// the emitted container carries them.
fn metadata_from_request(
    task: &InstallTask,
    request: &BuildRequest,
    scan: &moraine_build::SonameScan,
    build_time: Option<u64>,
) -> MetadataMap {
    let pkg = &request.package;
    let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
    let mut meta = MetadataMap::new();
    meta.set_str("CATEGORY", &pkg.ident.category);
    meta.set_str("PF", pf);
    meta.set_str("SLOT", &pkg.slot);
    meta.set_str("EAPI", &pkg.ident.eapi);
    meta.set_str("repository", &pkg.ident.repository);
    let mut use_flags: Vec<String> = request.use_flags.iter().cloned().collect();
    use_flags.sort();
    meta.set_str("USE", use_flags.join(" "));
    meta.set_str("IUSE", pkg.iuse.join(" "));
    meta.set_str("KEYWORDS", pkg.keywords.join(" "));
    meta.set_str("RESTRICT", pkg.restrict.join(" "));
    for key in [
        "DEPEND", "RDEPEND", "BDEPEND", "PDEPEND", "IDEPEND", "LICENSE",
    ] {
        if let Some(value) = pkg.reduced_meta.get(key) {
            meta.set_str(key, value);
        }
    }
    let provides = render_soname_field(&scan.provides);
    if !provides.is_empty() {
        meta.set_str("PROVIDES", &provides);
    }
    let requires = render_soname_field(&scan.requires);
    if !requires.is_empty() {
        meta.set_str("REQUIRES", &requires);
    }
    if let Some(bt) = build_time {
        meta.set_str("BUILD_TIME", bt.to_string());
    }
    meta
}

/// Render `(bucket, soname)` pairs into Portage's `bucket: soname soname` field
/// form, the inverse of `moraine_binpkg::parse_sonames`.
fn render_soname_field(pairs: &[(String, String)]) -> String {
    let mut by_bucket: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for (bucket, soname) in pairs {
        by_bucket.entry(bucket).or_default().push(soname);
    }
    let mut out = Vec::new();
    for (bucket, sonames) in by_bucket {
        out.push(format!("{bucket}: {}", sonames.join(" ")));
    }
    out.join(" ")
}

/// Unpack a binary package and build the merge operation for `task`, staging the
/// image under `stage_dir`.
///
/// The container is read under the given signature `policy` and optional key
/// `signature` config, so `binpkg-request-signature` makes an unsigned package
/// fatal and an inline-signed Manifest is gpg-verified before its contents are
/// trusted.
pub fn realize_binpkg(
    bytes: &[u8],
    task: &InstallTask,
    stage_dir: &Path,
    signature: Option<&moraine_binpkg::SignatureConfig>,
    policy: moraine_binpkg::SignaturePolicy,
) -> Result<Realized> {
    let pkg =
        read_package_with_policy(bytes, signature, policy).map_err(|e| InstallError::Realize {
            cpv: task.cpv.clone(),
            reason: format!("could not read binary package: {e}"),
        })?;

    let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
    let image_dir = stage_dir.join(pf);
    std::fs::create_dir_all(&image_dir).map_err(|e| InstallError::io(&image_dir, e))?;
    stage_image(&pkg.image, &image_dir)?;

    let state = state_from_metadata(task, &pkg.metadata);
    let op = MergeOp {
        image_dir,
        state,
        replaces: task.replaces.clone(),
        world_atom: task.world_atom.clone(),
        // A binary package's stored build-time elog is not yet surfaced here.
        elog: Vec::new(),
        // The ebuild copy from the binpkg is not surfaced on this path yet.
        ebuild: None,
    };
    Ok(Realized::Apply(Operation::Merge(Box::new(op))))
}

/// Stage a binary package's image tar into `dest`: decompress the stream when it
/// carries a recognized compression header (a real Portage `tar.bz2`/`tar.xz`
/// xpak image), then extract each member with any leading `image/` arcname
/// component stripped, mirroring Portage's `tar_safe_extract(image, "image")`.
/// Both the gpkg and xpak paths go through this one implementation.
fn stage_image(image: &[u8], dest: &Path) -> Result<()> {
    let decompressed = maybe_decompress(image)?;
    let mut archive = tar::Archive::new(decompressed.as_ref());
    archive.set_preserve_permissions(true);
    let entries = archive
        .entries()
        .map_err(|e| InstallError::io(dest, std::io::Error::other(e.to_string())))?;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| InstallError::io(dest, std::io::Error::other(e.to_string())))?;
        let path = entry
            .path()
            .map_err(|e| InstallError::io(dest, std::io::Error::other(e.to_string())))?
            .into_owned();
        let rel: PathBuf = path
            .strip_prefix("image")
            .map(Path::to_path_buf)
            .unwrap_or(path);
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out = dest.join(&rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| InstallError::io(parent, e))?;
        }
        entry.unpack(&out).map_err(|e| InstallError::io(&out, e))?;
    }
    Ok(())
}

/// Decompress an image stream when it begins with a recognized compression
/// header (bzip2, gzip, zstd, xz); otherwise return it unchanged (a plain tar).
fn maybe_decompress(bytes: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>> {
    use moraine_binpkg::Compression;
    let comp = match bytes {
        [0x42, 0x5a, 0x68, ..] => Some(Compression::Bzip2), // "BZh"
        [0x1f, 0x8b, ..] => Some(Compression::Gzip),        // gzip
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Some(Compression::Zstd), // zstd
        [0xfd, b'7', b'z', b'X', b'Z', 0x00, ..] => Compression::from_suffix("xz").ok(),
        _ => None,
    };
    match comp {
        Some(c) => {
            let out = c.decompress(bytes).map_err(|e| InstallError::Realize {
                cpv: "binpkg image".to_string(),
                reason: format!("could not decompress image: {e}"),
            })?;
            Ok(std::borrow::Cow::Owned(out))
        }
        None => Ok(std::borrow::Cow::Borrowed(bytes)),
    }
}

/// Build the installed-state record from the task identity and container
/// metadata. Identity comes from the resolved task; the remaining recorded
/// fields come from the binary package's metadata map.
fn state_from_metadata(task: &InstallTask, meta: &MetadataMap) -> PackageState {
    let (category, package) = task.cp.split_once('/').unwrap_or((task.cp.as_str(), ""));
    let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
    let version = pf
        .strip_prefix(&format!("{package}-"))
        .unwrap_or(pf)
        .to_owned();

    let split = |key: &str| -> Vec<String> {
        meta.get_str(key)
            .map(|s| s.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let scalar = |key: &str| meta.get_str(key).unwrap_or_default();

    let mut depends = std::collections::BTreeMap::new();
    for key in ["DEPEND", "RDEPEND", "BDEPEND", "PDEPEND", "IDEPEND"] {
        if let Some(value) = meta.get_str(key)
            && !value.trim().is_empty()
        {
            depends.insert(key.to_owned(), value);
        }
    }

    // The recorded SLOT may carry a sub-slot as `slot/subslot`.
    let slot_raw = if task.slot.is_empty() {
        scalar("SLOT")
    } else {
        task.slot.clone()
    };
    let (slot, subslot) = split_slot(&slot_raw);

    let sonames = |key: &str| -> Vec<moraine_merge::state::Soname> {
        meta.get_str(key)
            .map(|raw| {
                moraine_binpkg::resolution::parse_sonames(&raw)
                    .into_iter()
                    .map(|(bucket, soname)| moraine_merge::state::Soname { bucket, soname })
                    .collect()
            })
            .unwrap_or_default()
    };

    PackageState {
        cpv: format!("{category}/{pf}"),
        category: category.to_owned(),
        package: package.to_owned(),
        version,
        eapi: meta.get_str("EAPI").unwrap_or_else(|| "0".to_owned()),
        slot,
        subslot,
        use_flags: split("USE"),
        iuse: split("IUSE"),
        depends,
        keywords: split("KEYWORDS"),
        license: scalar("LICENSE"),
        description: scalar("DESCRIPTION"),
        homepage: scalar("HOMEPAGE"),
        properties: scalar("PROPERTIES"),
        restrict: scalar("RESTRICT"),
        repository: meta.get_str("repository"),
        defined_phases: split("DEFINED_PHASES"),
        build_time: meta
            .get_str("BUILD_TIME")
            .and_then(|s| s.trim().parse().ok()),
        chost: scalar("CHOST"),
        provides: sonames("PROVIDES"),
        requires: sonames("REQUIRES"),
        environment: None,
        inherited: split("INHERITED"),
        features: split("FEATURES"),
        size: meta.get_str("SIZE").and_then(|s| s.trim().parse().ok()),
        build_id: meta.get_str("BUILD_ID").and_then(|s| s.trim().parse().ok()),
        needed: Vec::new(),
        toolchain: moraine_vdb::record::Toolchain {
            cbuild: scalar("CBUILD"),
            cc: scalar("CC"),
            cflags: scalar("CFLAGS"),
            cxx: scalar("CXX"),
            cxxflags: scalar("CXXFLAGS"),
            ctarget: scalar("CTARGET"),
            asflags: scalar("ASFLAGS"),
            ldflags: scalar("LDFLAGS"),
        },
    }
}

/// Split a recorded `SLOT` of the form `slot` or `slot/subslot` into its parts.
fn split_slot(raw: &str) -> (String, Option<String>) {
    match raw.split_once('/') {
        Some((slot, sub)) => (slot.to_owned(), Some(sub.to_owned())),
        None => (raw.to_owned(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moraine_binpkg::greenfield::{WriteOptions, write_bytes};

    fn make_binpkg() -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        let data = b"#!/bin/sh\n";
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "usr/bin/foo", data.as_slice())
            .unwrap();
        let image = builder.into_inner().unwrap();

        let mut meta = MetadataMap::new();
        meta.set_str("EAPI", "8");
        meta.set_str("SLOT", "0");
        meta.set_str("USE", "ssl zlib");
        meta.set_str("RDEPEND", "dev-libs/openssl");
        write_bytes(&meta, &image, &WriteOptions::default()).unwrap()
    }

    #[test]
    fn realize_binpkg_unpacks_and_builds_op() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = make_binpkg();
        let task = {
            let mut t = InstallTask::merge("app/foo-1.2", "app/foo", "0");
            t.source = SourceKind::Binary;
            t.world_atom = Some("app/foo".to_owned());
            t
        };
        let realized = realize_binpkg(&bytes, &task, dir.path(), None, Default::default()).unwrap();
        let Realized::Apply(Operation::Merge(op)) = realized else {
            panic!("expected a merge op");
        };
        assert_eq!(op.world_atom.as_deref(), Some("app/foo"));
        assert_eq!(op.state.version, "1.2");
        assert_eq!(
            op.state.use_flags,
            vec!["ssl".to_owned(), "zlib".to_owned()]
        );
        assert_eq!(op.state.eapi, "8");
        assert!(op.image_dir.join("usr/bin/foo").exists());
    }

    #[test]
    fn gpkg_and_xpak_stage_root_relative() {
        // Build a root-relative image tar shared by both formats.
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        let data = b"x";
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "usr/bin/foo", data.as_slice())
            .unwrap();
        let image_tar = builder.into_inner().unwrap();
        let mut meta = MetadataMap::new();
        meta.set_str("EAPI", "8");
        meta.set_str("SLOT", "0");
        meta.set_str("PF", "foo-1.2");

        for format in [
            moraine_binpkg::BinpkgFormat::Gpkg,
            moraine_binpkg::BinpkgFormat::Xpak,
        ] {
            let bytes = format
                .write(&meta, &image_tar, moraine_binpkg::Compression::Bzip2)
                .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let mut task = InstallTask::merge("app/foo-1.2", "app/foo", "0");
            task.source = SourceKind::Binary;
            let realized =
                realize_binpkg(&bytes, &task, dir.path(), None, Default::default()).unwrap();
            let Realized::Apply(Operation::Merge(op)) = realized else {
                panic!("expected a merge op for {format:?}");
            };
            // The `image/` arcname (gpkg) is stripped; xpak is already root-relative.
            assert!(
                op.image_dir.join("usr/bin/foo").exists(),
                "staged tree must be root-relative for {format:?}"
            );
            assert!(
                !op.image_dir.join("image/usr/bin/foo").exists(),
                "the image/ prefix must be stripped for {format:?}"
            );
        }
    }

    #[test]
    fn request_signature_makes_unsigned_install_fatal() {
        // A plain gpkg (unsigned Manifest) must fail to install under
        // `RequestSignature`, but install normally under the default policy.
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        let data = b"x";
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "usr/bin/foo", data.as_slice())
            .unwrap();
        let image_tar = builder.into_inner().unwrap();
        let mut meta = MetadataMap::new();
        meta.set_str("EAPI", "8");
        meta.set_str("SLOT", "0");
        meta.set_str("PF", "foo-1.2");
        let bytes = moraine_binpkg::BinpkgFormat::Gpkg
            .write(&meta, &image_tar, moraine_binpkg::Compression::Gzip)
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let mut task = InstallTask::merge("app/foo-1.2", "app/foo", "0");
        task.source = SourceKind::Binary;

        assert!(
            realize_binpkg(
                &bytes,
                &task,
                dir.path(),
                None,
                moraine_binpkg::SignaturePolicy::RequestSignature,
            )
            .is_err(),
            "request-signature rejects an unsigned package"
        );
        assert!(
            realize_binpkg(
                &bytes,
                &task,
                dir.path(),
                None,
                moraine_binpkg::SignaturePolicy::default(),
            )
            .is_ok(),
            "the default policy installs an unsigned package"
        );
    }

    #[test]
    fn source_task_is_rejected_with_reason() {
        let dir = tempfile::tempdir().unwrap();
        let runner = BinpkgRunner::new(
            LocalPkgdir {
                pkgdir: dir.path().to_path_buf(),
            },
            dir.path().join("stage"),
        );
        let task = InstallTask::merge("app/foo-1", "app/foo", "0");
        let err = runner.realize(&task).unwrap_err();
        assert!(matches!(err, InstallError::Realize { .. }));
    }

    #[test]
    fn binhost_absent_container_is_none() {
        let dir = tempfile::tempdir().unwrap();
        // An empty base URI yields nothing.
        let empty = BinhostSource {
            base_uri: String::new(),
            fetch: moraine_binpkg::fetch::FetchCommand::default(),
            stage_dir: dir.path().join("stage"),
        };
        let mut task = InstallTask::merge("app/foo-1", "app/foo", "0");
        task.source = SourceKind::Binary;
        assert!(empty.fetch(&task).unwrap().is_none());

        // A failing fetch command reports the container as unavailable.
        let failing = BinhostSource {
            base_uri: "http://example.invalid".to_owned(),
            fetch: moraine_binpkg::fetch::FetchCommand {
                command: "false".to_owned(),
                args: vec![],
            },
            stage_dir: dir.path().join("stage"),
        };
        assert!(failing.fetch(&task).unwrap().is_none());
    }

    #[test]
    fn local_pkgdir_finds_container() {
        let dir = tempfile::tempdir().unwrap();
        let pkgdir = dir.path().join("pkgs");
        std::fs::create_dir_all(pkgdir.join("app")).unwrap();
        std::fs::write(pkgdir.join("app/foo-1.2.gpkg.tar"), make_binpkg()).unwrap();
        let source = LocalPkgdir {
            pkgdir: pkgdir.clone(),
        };
        let mut task = InstallTask::merge("app/foo-1.2", "app/foo", "0");
        task.source = SourceKind::Binary;
        assert!(source.fetch(&task).unwrap().is_some());
        let mut missing = InstallTask::merge("app/bar-9", "app/bar", "0");
        missing.source = SourceKind::Binary;
        assert!(source.fetch(&missing).unwrap().is_none());
    }

    #[test]
    fn local_pkgdir_finds_single_and_multi_instance() {
        let dir = tempfile::tempdir().unwrap();
        let pkgdir = dir.path().join("pkgs");
        // Single-instance: <category>/<pf>.gpkg.tar
        std::fs::create_dir_all(pkgdir.join("app")).unwrap();
        std::fs::write(pkgdir.join("app/foo-1.2.gpkg.tar"), make_binpkg()).unwrap();
        // Multi-instance: <cp>/<pf>-<buildid>.gpkg.tar
        std::fs::create_dir_all(pkgdir.join("app/bar")).unwrap();
        std::fs::write(pkgdir.join("app/bar/bar-2-1.gpkg.tar"), make_binpkg()).unwrap();
        std::fs::write(pkgdir.join("app/bar/bar-2-3.gpkg.tar"), make_binpkg()).unwrap();

        let single = locate_local_gpkg(&pkgdir, "app/foo", "app/foo-1.2").unwrap();
        assert!(single.ends_with("app/foo-1.2.gpkg.tar"));

        // The newest build id wins for the multi-instance layout.
        let multi = locate_local_gpkg(&pkgdir, "app/bar", "app/bar-2").unwrap();
        assert!(multi.ends_with("app/bar/bar-2-3.gpkg.tar"));

        assert!(locate_local_gpkg(&pkgdir, "app/none", "app/none-9").is_none());
    }
}
