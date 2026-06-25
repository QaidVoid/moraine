//! Filesystem helpers: atomic writes and memory-mapped reads.

use std::io::Write as _;
use std::path::Path;

use memmap2::Mmap;

use crate::error::{CommonError, IoResultExt as _};

/// Atomically write `bytes` to `path`.
///
/// The data is written to a temporary file in the same directory, flushed to
/// disk, and then renamed into place. A concurrent reader therefore sees either
/// the previous file or the complete new file, never a partial write.
pub fn atomic_write(path: impl AsRef<Path>, bytes: &[u8]) -> Result<(), CommonError> {
    let path = path.as_ref();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).with_path(dir)?;
    tmp.write_all(bytes).with_path(path)?;
    // `tempfile` creates the file mode 0600; relax it to 0644 so caches and
    // stores written as root stay readable by other users, matching Portage's
    // world-readable vdb and metadata caches.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644))
            .with_path(path)?;
    }
    tmp.as_file().sync_all().with_path(path)?;
    tmp.persist(path).map_err(|err| CommonError::Io {
        path: path.to_path_buf(),
        source: err.error,
    })?;
    Ok(())
}

/// Memory-map a file read-only.
///
/// The caller must ensure the file is not modified or truncated by another
/// process while the returned map is alive, since that is undefined behavior
/// for memory-mapped files.
pub fn mmap_read(path: impl AsRef<Path>) -> Result<Mmap, CommonError> {
    let path = path.as_ref();
    let file = std::fs::File::open(path).with_path(path)?;
    // SAFETY: see the documented contract; the file is opened read-only here.
    let map = unsafe { Mmap::map(&file) }.with_path(path)?;
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_then_mmap_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let payload = b"moraine roundtrip";

        atomic_write(&path, payload).unwrap();
        let map = mmap_read(&path).unwrap();

        assert_eq!(&map[..], payload);
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");

        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }
}
