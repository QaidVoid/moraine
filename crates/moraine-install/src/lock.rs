//! The transaction lock.
//!
//! A transaction holds an exclusive lock for its whole duration so two
//! transactions never mutate installed state concurrently. The lock is taken
//! fail-fast: if another live transaction holds it, acquisition fails rather
//! than blocking. The lock records the owning process id, so a lock left behind
//! by a process that was killed (where the drop never ran) is detected as stale
//! and reclaimed rather than blocking forever.

use std::io::Write as _;
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

    /// Acquire the lock under `state_dir`, failing if a live transaction holds
    /// it. A lock owned by a dead process is reclaimed.
    pub fn acquire(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir).map_err(|e| InstallError::io(state_dir, e))?;
        let path = Self::path_in(state_dir);
        // Two attempts: the second runs only after reclaiming a stale lock, so a
        // live holder still fails fast.
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let _ = write!(file, "{}", std::process::id());
                    return Ok(TransactionLock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && reclaim_if_stale(&path) {
                        continue;
                    }
                    return Err(InstallError::Locked { path });
                }
                Err(e) => return Err(InstallError::io(path, e)),
            }
        }
        Err(InstallError::Locked { path })
    }
}

/// Remove the lock file when its recorded owner is no longer running, returning
/// whether it was reclaimed.
fn reclaim_if_stale(path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        // An unreadable or empty owner id: treat as stale and reclaim.
        return std::fs::remove_file(path).is_ok();
    };
    if process_alive(pid) {
        return false;
    }
    std::fs::remove_file(path).is_ok()
}

/// Whether a process with `pid` is currently running, via `/proc` on Linux.
fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

impl Drop for TransactionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
