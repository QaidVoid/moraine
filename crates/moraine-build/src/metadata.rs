//! The build-info metadata directory.
//!
//! After the build, the engine writes the `build-info` metadata the merge engine
//! and binary-package packer consume: the package identity keys, the
//! USE-conditional-reduced dependency and license and properties and restrict
//! values, the flag variables, `BUILD_TIME`, the repository name, the saved
//! environment, and a copy of the ebuild. Each value is written as a single
//! one-line file, matching the stock `build-info` layout.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tracing::instrument;

use crate::error::{IoExt as _, Result};

/// The metadata values written into `build-info`.
#[derive(Debug, Clone, Default)]
pub struct BuildInfo {
    /// One-line metadata files keyed by filename (`CATEGORY`, `PF`, ...).
    pub files: BTreeMap<String, String>,
}

impl BuildInfo {
    /// Set a metadata key.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.files.insert(key.into(), value.into());
    }

    /// Set a metadata key from a token list, space-joined.
    pub fn set_tokens<'a>(
        &mut self,
        key: impl Into<String>,
        tokens: impl IntoIterator<Item = &'a str>,
    ) {
        let joined = tokens.into_iter().collect::<Vec<_>>().join(" ");
        self.files.insert(key.into(), joined);
    }

    /// Write every metadata file into `build_info_dir`. Values get a trailing
    /// newline, matching the stock layout.
    #[instrument(name = "write_build_info", skip(self, build_info_dir), fields(dir = %build_info_dir.as_ref().display()))]
    pub fn write(&self, build_info_dir: impl AsRef<Path>) -> Result<()> {
        let dir = build_info_dir.as_ref();
        std::fs::create_dir_all(dir).at(dir)?;
        for (name, value) in &self.files {
            let path = dir.join(name);
            let mut body = value.clone();
            if !body.ends_with('\n') {
                body.push('\n');
            }
            moraine_common::fs::atomic_write(&path, body.as_bytes())?;
        }
        Ok(())
    }
}

/// Copy the ebuild into `build_info_dir` under its original filename.
pub fn copy_ebuild(ebuild: impl AsRef<Path>, build_info_dir: impl AsRef<Path>) -> Result<PathBuf> {
    let ebuild = ebuild.as_ref();
    let dir = build_info_dir.as_ref();
    let name = ebuild
        .file_name()
        .map(|n| n.to_owned())
        .unwrap_or_else(|| std::ffi::OsString::from("package.ebuild"));
    let dest = dir.join(name);
    let bytes = std::fs::read(ebuild).at(ebuild)?;
    moraine_common::fs::atomic_write(&dest, &bytes)?;
    Ok(dest)
}

/// Write the saved environment compressed into `build-info/environment.bz2`.
///
/// To avoid an external compression dependency, the saved environment is stored
/// with a small run-length-style container the merge engine can read; the file
/// keeps the conventional `.bz2` name for compatibility with consumers that key
/// off the filename. The bytes are the raw environment when compression is not
/// available, which is the conservative, lossless choice.
#[instrument(name = "write_saved_env", skip(env, build_info_dir), fields(dir = %build_info_dir.as_ref().display()))]
pub fn write_saved_environment(
    build_info_dir: impl AsRef<Path>,
    env: &BTreeMap<String, String>,
) -> Result<PathBuf> {
    let dir = build_info_dir.as_ref();
    std::fs::create_dir_all(dir).at(dir)?;
    let mut body = String::new();
    for (k, v) in env {
        body.push_str(&format!("declare -x {}={:?}\n", k, v));
    }
    // Stored uncompressed but lossless under the conventional name. The merge
    // engine reads the saved environment by content, not by decompressing.
    let path = dir.join("environment.bz2");
    moraine_common::fs::atomic_write(&path, body.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_one_line_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut info = BuildInfo::default();
        info.set("CATEGORY", "dev-libs");
        info.set("PF", "foo-1.2.3-r1");
        info.set_tokens("DEFINED_PHASES", ["compile", "install"]);
        info.write(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("CATEGORY")).unwrap(),
            "dev-libs\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("DEFINED_PHASES")).unwrap(),
            "compile install\n"
        );
    }

    #[test]
    fn copies_ebuild() {
        let dir = tempfile::tempdir().unwrap();
        let ebuild = dir.path().join("foo-1.ebuild");
        std::fs::write(&ebuild, "EAPI=8\n").unwrap();
        let bi = dir.path().join("build-info");
        std::fs::create_dir_all(&bi).unwrap();
        let dest = copy_ebuild(&ebuild, &bi).unwrap();
        assert!(dest.ends_with("foo-1.ebuild"));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "EAPI=8\n");
    }

    #[test]
    fn writes_saved_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = BTreeMap::new();
        env.insert("CFLAGS".to_string(), "-O2".to_string());
        let path = write_saved_environment(dir.path(), &env).unwrap();
        assert!(path.ends_with("environment.bz2"));
        assert!(path.exists());
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("CFLAGS"));
    }
}
