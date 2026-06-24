//! The transaction lock.
//!
//! A transaction holds an exclusive lock for its whole duration so two
//! transactions never mutate installed state concurrently. The lock is taken
//! fail-fast: if another transaction holds it, acquisition fails rather than
//! blocking. It is released when the guard drops, including on failure or
//! interruption.

use std::path::{Path, PathBuf};

use crate::error::{InstallError, Result};

/// A held transaction lock, released on drop.
#[derive(Debug)]
pub struct TransactionLock {
    path: PathBuf,
}

impl TransactionLock {
    /// The lock file path under a state directory.
    pub fn path_in(state_dir: &Path) -> PathBuf {
        state_dir.join("transaction.lock")
    }

    /// Acquire the lock under `state_dir`, failing if it is already held.
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir).map_err(|e| InstallError::io(state_dir, e))?;
        let path = Self::path_in(state_dir);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(TransactionLock { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(InstallError::Locked { path })
            }
            Err(e) => Err(InstallError::io(path, e)),
        }
    }
}

impl Drop for TransactionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
