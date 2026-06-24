//! Build-tree layout.
//!
//! Lays out the per-package build directory under the configured build root,
//! mirroring the stock `PORTAGE_BUILDDIR = $PORTAGE_TMPDIR/portage/$CATEGORY/$PF`
//! layout with its `work`, `temp`, `image`, `homedir`, `build-info`, and `.ipc`
//! subdirectories. Every path is confined under the configured build root.

use std::path::{Path, PathBuf};

use tracing::instrument;

use crate::error::{BuildError, IoExt as _, Result};

/// The on-disk layout of one package's build directory.
#[derive(Debug, Clone)]
pub struct BuildLayout {
    /// The package build directory `$build_root/portage/$CATEGORY/$PF`.
    pub builddir: PathBuf,
    /// The working tree, `WORKDIR` (`work`).
    pub workdir: PathBuf,
    /// The temporary directory, `T` (`temp`).
    pub temp: PathBuf,
    /// The image directory, `D` (`image`).
    pub image: PathBuf,
    /// The build home, `HOME` (`homedir`).
    pub home: PathBuf,
    /// The build-info metadata directory (`build-info`).
    pub build_info: PathBuf,
    /// The IPC directory (`.ipc`).
    pub ipc: PathBuf,
    /// The build log path under `temp` (`build.log`).
    pub build_log: PathBuf,
    root: PathBuf,
}

impl BuildLayout {
    /// Compute the layout for a package under `build_root` (the configured
    /// `PORTAGE_TMPDIR`). This does not touch the filesystem; call
    /// [`BuildLayout::create`] to materialize it.
    pub fn new(build_root: impl AsRef<Path>, category: &str, pf: &str) -> Result<Self> {
        if category.is_empty() || pf.is_empty() {
            return Err(BuildError::environment(
                "category and PF must be non-empty for the build layout",
            ));
        }
        if category.contains('/') || pf.contains('/') {
            return Err(BuildError::environment(
                "category and PF must not contain a path separator",
            ));
        }
        let root = build_root.as_ref().to_path_buf();
        let builddir = root.join("portage").join(category).join(pf);
        let temp = builddir.join("temp");
        Ok(BuildLayout {
            workdir: builddir.join("work"),
            temp: temp.clone(),
            image: builddir.join("image"),
            home: builddir.join("homedir"),
            build_info: builddir.join("build-info"),
            ipc: builddir.join(".ipc"),
            build_log: temp.join("build.log"),
            builddir,
            root,
        })
    }

    /// Create every directory in the layout. The image directory is recreated
    /// empty so a stale image from a previous build cannot leak into this one.
    #[instrument(name = "layout_create", skip(self), fields(builddir = %self.builddir.display()))]
    pub fn create(&self) -> Result<()> {
        // The image must start empty for each build.
        if self.image.exists() {
            std::fs::remove_dir_all(&self.image).at(&self.image)?;
        }
        for dir in [
            &self.builddir,
            &self.workdir,
            &self.temp,
            &self.image,
            &self.home,
            &self.build_info,
            &self.ipc,
        ] {
            self.assert_confined(dir)?;
            std::fs::create_dir_all(dir).at(dir)?;
        }
        Ok(())
    }

    /// The configured build root all layout paths are confined to.
    pub fn build_root(&self) -> &Path {
        &self.root
    }

    /// Verify a path is under the configured build root, refusing to create
    /// anything outside it.
    fn assert_confined(&self, path: &Path) -> Result<()> {
        if !path.starts_with(&self.root) {
            return Err(BuildError::environment(format!(
                "build path {} escapes the build root {}",
                path.display(),
                self.root.display()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_paths_are_under_root() {
        let tmp = tempfile::tempdir().unwrap();
        let l = BuildLayout::new(tmp.path(), "dev-libs", "foo-1.0").unwrap();
        for p in [
            &l.builddir,
            &l.workdir,
            &l.temp,
            &l.image,
            &l.home,
            &l.build_info,
            &l.ipc,
        ] {
            assert!(p.starts_with(tmp.path()));
        }
        assert!(l.builddir.ends_with("portage/dev-libs/foo-1.0"));
    }

    #[test]
    fn create_makes_all_dirs_and_empty_image() {
        let tmp = tempfile::tempdir().unwrap();
        let l = BuildLayout::new(tmp.path(), "dev-libs", "foo-1.0").unwrap();
        l.create().unwrap();
        for p in [
            &l.workdir,
            &l.temp,
            &l.image,
            &l.home,
            &l.build_info,
            &l.ipc,
        ] {
            assert!(p.is_dir(), "{} not created", p.display());
        }
        // Image starts empty.
        assert_eq!(std::fs::read_dir(&l.image).unwrap().count(), 0);
    }

    #[test]
    fn create_clears_stale_image() {
        let tmp = tempfile::tempdir().unwrap();
        let l = BuildLayout::new(tmp.path(), "dev-libs", "foo-1.0").unwrap();
        l.create().unwrap();
        std::fs::write(l.image.join("stale"), b"old").unwrap();
        l.create().unwrap();
        assert_eq!(std::fs::read_dir(&l.image).unwrap().count(), 0);
    }

    #[test]
    fn rejects_path_separator_in_components() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(BuildLayout::new(tmp.path(), "dev/libs", "foo-1.0").is_err());
        assert!(BuildLayout::new(tmp.path(), "dev-libs", "foo/1.0").is_err());
    }
}
