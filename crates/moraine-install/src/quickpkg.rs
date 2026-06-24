//! Binary-package creation: quickpkg and build-byproduct packaging.
//!
//! [`QuickpkgInput`] archives an already-installed package's recorded files into
//! a binary package without building. [`package_image_dir`] writes a freshly
//! built image directory as a binary package, used for `--buildpkg` and
//! `--buildpkgonly`. Both emit the greenfield container format through
//! `moraine-binpkg`.

use std::path::{Path, PathBuf};

use moraine_binpkg::MetadataMap;
use moraine_binpkg::greenfield::{self, WriteOptions};

use crate::error::{InstallError, Result};

/// The inputs to quickpkg: an installed package's files and metadata.
#[derive(Debug, Clone)]
pub struct QuickpkgInput {
    /// The `category/package-version` being packaged.
    pub cpv: String,
    /// The live install root the recorded files live under.
    pub eroot: PathBuf,
    /// The recorded install paths (absolute, for example `/usr/bin/foo`).
    pub files: Vec<String>,
    /// The package metadata to embed in the container.
    pub metadata: MetadataMap,
}

impl QuickpkgInput {
    /// Build the uncompressed image tar from the recorded files, reading each
    /// from the live root. Missing files are skipped, matching `quickpkg`'s
    /// tolerance of files removed since install.
    pub fn image_tar(&self) -> Result<Vec<u8>> {
        let mut builder = tar::Builder::new(Vec::new());
        for install_path in &self.files {
            let rel = install_path.trim_start_matches('/');
            let live = self.eroot.join(rel);
            let meta = match std::fs::symlink_metadata(&live) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(InstallError::io(&live, e)),
            };
            if meta.is_dir() {
                continue;
            }
            builder
                .append_path_with_name(&live, rel)
                .map_err(|e| InstallError::io(&live, e))?;
        }
        builder
            .into_inner()
            .map_err(|e| InstallError::io(&self.eroot, e))
    }

    /// Write the binary package to `out_path`.
    pub fn write(&self, out_path: &Path, options: &WriteOptions) -> Result<()> {
        let image = self.image_tar()?;
        greenfield::write_file(out_path, &self.metadata, &image, options).map_err(|e| {
            InstallError::Realize {
                cpv: self.cpv.clone(),
                reason: format!("failed to write binary package: {e}"),
            }
        })
    }
}

/// Write a built image directory as a binary package at `out_path`.
pub fn package_image_dir(
    cpv: &str,
    image_dir: &Path,
    metadata: &MetadataMap,
    out_path: &Path,
    options: &WriteOptions,
) -> Result<()> {
    let mut builder = tar::Builder::new(Vec::new());
    builder
        .append_dir_all(".", image_dir)
        .map_err(|e| InstallError::io(image_dir, e))?;
    let image = builder
        .into_inner()
        .map_err(|e| InstallError::io(image_dir, e))?;
    greenfield::write_file(out_path, metadata, &image, options).map_err(|e| InstallError::Realize {
        cpv: cpv.to_owned(),
        reason: format!("failed to write binary package: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quickpkg_archives_recorded_files() {
        let dir = tempfile::tempdir().unwrap();
        let eroot = dir.path();
        std::fs::create_dir_all(eroot.join("usr/bin")).unwrap();
        std::fs::write(eroot.join("usr/bin/foo"), b"binary").unwrap();

        let mut metadata = MetadataMap::new();
        metadata.set_str("CATEGORY", "app");
        let input = QuickpkgInput {
            cpv: "app/foo-1".into(),
            eroot: eroot.to_path_buf(),
            files: vec!["/usr/bin/foo".into(), "/usr/bin/missing".into()],
            metadata,
        };

        let out = eroot.join("foo.gpkg");
        input.write(&out, &WriteOptions::default()).unwrap();
        assert!(out.exists());

        let bytes = std::fs::read(&out).unwrap();
        let pkg = moraine_binpkg::read_package(&bytes, None).unwrap();
        assert_eq!(pkg.metadata.get_str("CATEGORY").as_deref(), Some("app"));
        let mut archive = tar::Archive::new(pkg.image.as_slice());
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "usr/bin/foo"));
        assert!(!names.iter().any(|n| n.contains("missing")));
    }

    #[test]
    fn package_image_dir_archives_tree() {
        let dir = tempfile::tempdir().unwrap();
        let image = dir.path().join("image");
        std::fs::create_dir_all(image.join("etc")).unwrap();
        std::fs::write(image.join("etc/conf"), b"x").unwrap();

        let out = dir.path().join("pkg.gpkg");
        package_image_dir(
            "app/foo-1",
            &image,
            &MetadataMap::new(),
            &out,
            &WriteOptions::default(),
        )
        .unwrap();
        assert!(out.exists());
    }
}
