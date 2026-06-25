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
use moraine_binpkg::{MetadataMap, read_package};
use moraine_build::{BuildRequest, CommandRunner, build_package};
use moraine_merge::state::PackageState;
use moraine_merge::{MergeOp, Operation};

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

/// A [`BinpkgSource`] backed by a local package directory laid out as
/// `<pkgdir>/<category>/<pf>.gpkg`.
pub struct LocalPkgdir {
    /// The package directory root (`PKGDIR`).
    pub pkgdir: PathBuf,
}

impl BinpkgSource for LocalPkgdir {
    fn fetch(&self, task: &InstallTask) -> Result<Option<Vec<u8>>> {
        let (category, _) = task.cp.split_once('/').unwrap_or((task.cp.as_str(), ""));
        let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
        let path = self.pkgdir.join(category).join(format!("{pf}.gpkg"));
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(InstallError::io(path, e)),
        }
    }
}

impl BinpkgSource for Box<dyn BinpkgSource> {
    fn fetch(&self, task: &InstallTask) -> Result<Option<Vec<u8>>> {
        (**self).fetch(task)
    }
}

/// A [`BinpkgSource`] that fetches containers from a binhost base URI, laid out
/// as `<base>/<category>/<pf>.gpkg`, into a staging directory.
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
            "{}/{}/{}.gpkg",
            self.base_uri.trim_end_matches('/'),
            category,
            pf
        );
        std::fs::create_dir_all(&self.stage_dir)
            .map_err(|e| InstallError::io(&self.stage_dir, e))?;
        let dest = self.stage_dir.join(format!("{pf}.gpkg"));
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
}

impl<S: BinpkgSource> BinpkgRunner<S> {
    /// Build a runner that stages unpacked images under `stage_dir`.
    pub fn new(source: S, stage_dir: impl Into<PathBuf>) -> Self {
        BinpkgRunner {
            source,
            stage_dir: stage_dir.into(),
        }
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
        realize_binpkg(&bytes, task, &self.stage_dir)
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
    /// The package directory to write binary packages into.
    pub pkgdir: PathBuf,
    /// The container write options.
    pub write_options: WriteOptions,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            buildpkg: false,
            buildpkgonly: false,
            pkgdir: PathBuf::from("/var/cache/binpkgs"),
            write_options: WriteOptions::default(),
        }
    }
}

/// A [`StepRunner`] that builds from source through the build engine.
pub struct SourceRunner<'r, P: BuildPlanner, R: CommandRunner> {
    planner: P,
    runner: &'r R,
    options: BuildOptions,
}

impl<'r, P: BuildPlanner, R: CommandRunner> SourceRunner<'r, P, R> {
    /// Build a source runner over `planner`, the external-command `runner`, and
    /// the binary-package `options`.
    pub fn new(planner: P, runner: &'r R, options: BuildOptions) -> Self {
        SourceRunner {
            planner,
            runner,
            options,
        }
    }
}

impl<P: BuildPlanner, R: CommandRunner> StepRunner for SourceRunner<'_, P, R> {
    fn realize(&self, task: &InstallTask) -> Result<Realized> {
        let request = self.planner.plan(task)?;
        let outcome = build_package(&request, self.runner).map_err(|e| InstallError::Realize {
            cpv: task.cpv.clone(),
            reason: format!("build failed: {e}"),
        })?;

        if self.options.buildpkg || self.options.buildpkgonly {
            let (category, _) = task.cp.split_once('/').unwrap_or((task.cp.as_str(), ""));
            let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
            let dir = self.options.pkgdir.join(category);
            std::fs::create_dir_all(&dir).map_err(|e| InstallError::io(&dir, e))?;
            let out = dir.join(format!("{pf}.gpkg"));
            let metadata = metadata_from_request(task, &request);
            package_image_dir(
                &task.cpv,
                &outcome.image_dir,
                &metadata,
                &out,
                &self.options.write_options,
            )?;
        }

        if self.options.buildpkgonly {
            return Ok(Realized::PackagedOnly);
        }

        let state = state_from_request(task, &request);
        let op = MergeOp {
            image_dir: outcome.image_dir,
            state,
            replaces: task.replaces.clone(),
            in_world: task.in_world,
        };
        Ok(Realized::Apply(Operation::Merge(Box::new(op))))
    }
}

/// Build the installed-state record from the task identity and the build
/// request's resolved package and USE set.
fn state_from_request(task: &InstallTask, request: &BuildRequest) -> PackageState {
    let pkg = &request.package;
    let (category, package) = task.cp.split_once('/').unwrap_or((task.cp.as_str(), ""));
    let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
    let version = pf
        .strip_prefix(&format!("{package}-"))
        .unwrap_or(pf)
        .to_owned();

    let mut use_flags: Vec<String> = request.use_flags.iter().cloned().collect();
    use_flags.sort();

    let mut depends = std::collections::BTreeMap::new();
    for key in ["DEPEND", "RDEPEND", "BDEPEND", "PDEPEND", "IDEPEND"] {
        if let Some(value) = pkg.reduced_meta.get(key)
            && !value.trim().is_empty()
        {
            depends.insert(key.to_owned(), value.clone());
        }
    }
    let meta = |key: &str| pkg.reduced_meta.get(key).cloned().unwrap_or_default();

    PackageState {
        cpv: task.cpv.clone(),
        category: category.to_owned(),
        package: package.to_owned(),
        version,
        eapi: pkg.ident.eapi.clone(),
        slot: pkg.slot.clone(),
        subslot: None,
        use_flags,
        iuse: pkg.iuse.clone(),
        depends,
        keywords: pkg.keywords.clone(),
        license: meta("LICENSE"),
        properties: meta("PROPERTIES"),
        restrict: pkg.restrict.join(" "),
        repository: Some(pkg.ident.repository.clone()),
        defined_phases: pkg.defined_phases.clone(),
        build_time: None,
        chost: request
            .config
            .vars
            .get("CHOST")
            .cloned()
            .unwrap_or_default(),
        provides: Vec::new(),
        requires: Vec::new(),
        environment: None,
    }
}

/// Build a binary-package metadata map from the build request, for the
/// `--buildpkg` byproduct.
fn metadata_from_request(task: &InstallTask, request: &BuildRequest) -> MetadataMap {
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
    meta
}

/// Unpack a binary package and build the merge operation for `task`, staging the
/// image under `stage_dir`.
pub fn realize_binpkg(bytes: &[u8], task: &InstallTask, stage_dir: &Path) -> Result<Realized> {
    let pkg = read_package(bytes, None).map_err(|e| InstallError::Realize {
        cpv: task.cpv.clone(),
        reason: format!("could not read binary package: {e}"),
    })?;

    let pf = task.cpv.rsplit('/').next().unwrap_or(&task.cpv);
    let image_dir = stage_dir.join(pf);
    std::fs::create_dir_all(&image_dir).map_err(|e| InstallError::io(&image_dir, e))?;
    let mut archive = tar::Archive::new(pkg.image.as_slice());
    archive
        .unpack(&image_dir)
        .map_err(|e| InstallError::io(&image_dir, e))?;

    let state = state_from_metadata(task, &pkg.metadata);
    let op = MergeOp {
        image_dir,
        state,
        replaces: task.replaces.clone(),
        in_world: task.in_world,
    };
    Ok(Realized::Apply(Operation::Merge(Box::new(op))))
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

    PackageState {
        cpv: format!("{category}/{pf}"),
        category: category.to_owned(),
        package: package.to_owned(),
        version,
        eapi: meta.get_str("EAPI").unwrap_or_else(|| "0".to_owned()),
        slot: if task.slot.is_empty() {
            scalar("SLOT")
        } else {
            task.slot.clone()
        },
        subslot: None,
        use_flags: split("USE"),
        iuse: split("IUSE"),
        depends,
        keywords: split("KEYWORDS"),
        license: scalar("LICENSE"),
        properties: scalar("PROPERTIES"),
        restrict: scalar("RESTRICT"),
        repository: meta.get_str("repository"),
        defined_phases: split("DEFINED_PHASES"),
        build_time: meta
            .get_str("BUILD_TIME")
            .and_then(|s| s.trim().parse().ok()),
        chost: scalar("CHOST"),
        provides: Vec::new(),
        requires: Vec::new(),
        environment: None,
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
            t.in_world = true;
            t
        };
        let realized = realize_binpkg(&bytes, &task, dir.path()).unwrap();
        let Realized::Apply(Operation::Merge(op)) = realized else {
            panic!("expected a merge op");
        };
        assert!(op.in_world);
        assert_eq!(op.state.version, "1.2");
        assert_eq!(
            op.state.use_flags,
            vec!["ssl".to_owned(), "zlib".to_owned()]
        );
        assert_eq!(op.state.eapi, "8");
        assert!(op.image_dir.join("usr/bin/foo").exists());
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
        std::fs::write(pkgdir.join("app/foo-1.2.gpkg"), make_binpkg()).unwrap();
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
}
