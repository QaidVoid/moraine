//! Scanning a built image directory into an ordered set of merge items.
//!
//! The image directory is the staged tree (`D`) produced by `moraine-build`.
//! Every entry under it maps to a target path within the install root. Items are
//! ordered so a directory always precedes the files and symlinks beneath it,
//! which lets the merge create directories before their contents.

use std::ffi::OsString;
use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};

use crate::error::{IoResultExt as _, MergeError};

/// One entry discovered in the image, with the install-root-relative path it
/// merges to, its kind, and the source metadata to reproduce on placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageItem {
    /// The absolute path within the install root (leading slash, no root offset).
    pub install_path: String,
    /// The absolute source path inside the image directory.
    pub source: PathBuf,
    /// The kind of entry.
    pub kind: ImageKind,
    /// The source `st_mode` (full mode word, type bits included).
    pub mode: u32,
    /// The source owner uid.
    pub uid: u32,
    /// The source owner gid.
    pub gid: u32,
    /// The source `st_rdev` for a device node, zero otherwise. Carries the device
    /// number to reproduce with `mknod`.
    pub rdev: u64,
    /// The source `st_dev`, paired with [`ino`](Self::ino) to detect hardlinks.
    pub dev: u64,
    /// The source `st_ino`. Two regular files sharing `(dev, ino)` are hardlinks
    /// of one another in the image and are re-created as hardlinks on placement.
    pub ino: u64,
    /// The source `st_nlink`; greater than one marks a possible hardlink.
    pub nlink: u64,
    /// The source modification time in whole seconds since the epoch, recorded in
    /// CONTENTS and stamped onto the placed file.
    pub mtime: i64,
    /// The nanosecond component of the source modification time, used to stamp the
    /// live file to full precision.
    pub mtime_nsec: i64,
    /// The captured extended attributes, when `FEATURES=xattr` is enabled. Empty
    /// otherwise and for symlinks, whose xattrs Portage does not copy.
    pub xattrs: Vec<(OsString, Vec<u8>)>,
}

/// The kind of an image entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageKind {
    /// A directory.
    Dir,
    /// A regular file.
    File,
    /// A symlink with the given recorded target.
    Sym {
        /// The link target exactly as stored in the image.
        target: String,
    },
    /// A named pipe (FIFO).
    Fifo,
    /// A character or block device node; the mode word distinguishes the two and
    /// `rdev` carries the device number.
    Dev,
}

/// Recursively scan `image_dir`, returning every entry ordered so that each
/// directory precedes its contents. Extended attributes are captured only when
/// `capture_xattrs` is set, matching `FEATURES=xattr`.
pub fn scan(image_dir: &Path, capture_xattrs: bool) -> Result<Vec<ImageItem>, MergeError> {
    if !image_dir.exists() {
        return Err(MergeError::MissingImage {
            path: image_dir.to_path_buf(),
        });
    }
    let mut out = Vec::new();
    walk(image_dir, image_dir, capture_xattrs, &mut out)?;
    // Sort by install path so directories sort before their children and the
    // result is deterministic regardless of readdir order.
    out.sort_by(|a, b| a.install_path.cmp(&b.install_path));
    Ok(out)
}

fn walk(
    root: &Path,
    dir: &Path,
    capture_xattrs: bool,
    out: &mut Vec<ImageItem>,
) -> Result<(), MergeError> {
    let read = std::fs::read_dir(dir).with_path(dir)?;
    for entry in read {
        let entry = entry.with_path(dir)?;
        let source = entry.path();
        let meta = std::fs::symlink_metadata(&source).with_path(&source)?;
        let install_path = install_path_of(root, &source)?;
        let (mode, uid, gid, rdev) = (meta.mode(), meta.uid(), meta.gid(), meta.rdev());
        let (dev, ino, nlink) = (meta.dev(), meta.ino(), meta.nlink());
        let (mtime, mtime_nsec) = (meta.mtime(), meta.mtime_nsec());
        let ft = meta.file_type();
        let is_symlink = ft.is_symlink();
        // Portage copies xattrs for regular files and directories, not symlinks.
        let xattrs = if capture_xattrs && !is_symlink {
            read_xattrs(&source)?
        } else {
            Vec::new()
        };
        let push = |out: &mut Vec<ImageItem>, source: PathBuf, kind: ImageKind| {
            out.push(ImageItem {
                install_path: install_path.clone(),
                source,
                kind,
                mode,
                uid,
                gid,
                rdev,
                dev,
                ino,
                nlink,
                mtime,
                mtime_nsec,
                xattrs: xattrs.clone(),
            });
        };
        if is_symlink {
            let target = std::fs::read_link(&source).with_path(&source)?;
            let target = normalize_abssymlink(root, &target);
            push(out, source, ImageKind::Sym { target });
        } else if ft.is_dir() {
            push(out, source.clone(), ImageKind::Dir);
            walk(root, &source, capture_xattrs, out)?;
        } else if ft.is_fifo() {
            push(out, source, ImageKind::Fifo);
        } else if ft.is_char_device() || ft.is_block_device() {
            push(out, source, ImageKind::Dev);
        } else {
            push(out, source, ImageKind::File);
        }
    }
    Ok(())
}

/// Normalize an image-internal absolute symlink target. An ebuild that creates
/// an absolute symlink pointing inside the staged image (`D`) leaks the build
/// path; Portage strips the image prefix so the recorded target is install-root
/// absolute. A target outside the image is returned unchanged.
fn normalize_abssymlink(root: &Path, target: &Path) -> String {
    if target.is_absolute()
        && let Ok(rel) = target.strip_prefix(root)
    {
        let mut p = String::from("/");
        p.push_str(&rel.to_string_lossy());
        return p;
    }
    target.to_string_lossy().into_owned()
}

/// Read every extended attribute name and value from `source`.
fn read_xattrs(source: &Path) -> Result<Vec<(OsString, Vec<u8>)>, MergeError> {
    let names = match xattr::list(source) {
        Ok(names) => names,
        // A filesystem without xattr support reports ENOTSUP; treat as none.
        Err(e) if e.raw_os_error() == Some(libc_enotsup()) => return Ok(Vec::new()),
        Err(source_err) => {
            return Err(MergeError::Io {
                path: source.to_path_buf(),
                source: source_err,
            });
        }
    };
    let mut out = Vec::new();
    for name in names {
        if let Some(value) = xattr::get(source, &name).with_path(source)? {
            out.push((name, value));
        }
    }
    Ok(out)
}

/// `ENOTSUP`, used to treat a filesystem without xattr support as carrying none.
fn libc_enotsup() -> i32 {
    // Linux ENOTSUP/EOPNOTSUPP.
    95
}

/// Compute the install-root-relative absolute path for an image entry.
fn install_path_of(root: &Path, source: &Path) -> Result<String, MergeError> {
    let rel = source.strip_prefix(root).map_err(|_| MergeError::Io {
        path: source.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "image entry escaped the image directory",
        ),
    })?;
    let mut p = String::from("/");
    p.push_str(&rel.to_string_lossy());
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_orders_dirs_before_contents() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path();
        std::fs::create_dir_all(img.join("usr/bin")).unwrap();
        std::fs::write(img.join("usr/bin/foo"), b"x").unwrap();
        std::os::unix::fs::symlink("foo", img.join("usr/bin/bar")).unwrap();

        let items = scan(img, false).unwrap();
        let paths: Vec<&str> = items.iter().map(|i| i.install_path.as_str()).collect();
        assert!(paths.contains(&"/usr"));
        assert!(paths.contains(&"/usr/bin"));
        assert!(paths.contains(&"/usr/bin/foo"));

        let usr = paths.iter().position(|p| *p == "/usr").unwrap();
        let usr_bin = paths.iter().position(|p| *p == "/usr/bin").unwrap();
        let foo = paths.iter().position(|p| *p == "/usr/bin/foo").unwrap();
        assert!(usr < usr_bin && usr_bin < foo);

        let bar = items
            .iter()
            .find(|i| i.install_path == "/usr/bin/bar")
            .unwrap();
        assert_eq!(
            bar.kind,
            ImageKind::Sym {
                target: "foo".to_string()
            }
        );
    }

    #[test]
    fn abssymlink_target_inside_image_is_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("image");
        std::fs::create_dir_all(img.join("usr/lib")).unwrap();
        std::fs::write(img.join("usr/lib/libfoo.so.1"), b"x").unwrap();
        // An absolute symlink leaking the image (D) path.
        let leaked = img.join("usr/lib/libfoo.so");
        std::os::unix::fs::symlink(img.join("usr/lib/libfoo.so.1"), &leaked).unwrap();

        let items = scan(&img, false).unwrap();
        let link = items
            .iter()
            .find(|i| i.install_path == "/usr/lib/libfoo.so")
            .unwrap();
        assert_eq!(
            link.kind,
            ImageKind::Sym {
                target: "/usr/lib/libfoo.so.1".to_string()
            },
            "image prefix stripped from absolute target"
        );
    }

    #[test]
    fn missing_image_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(matches!(
            scan(&missing, false),
            Err(MergeError::MissingImage { .. })
        ));
    }
}
