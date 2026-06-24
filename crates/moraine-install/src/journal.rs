//! The resume journal.
//!
//! Before a transaction starts, the orchestrator writes the full task list to a
//! journal under the state directory and trims it as each package commits. A
//! fresh run clears any stale journal and writes a new one; `--resume` reads the
//! journal and replays the tasks that did not complete. The journal is the
//! cross-package counterpart to the merge engine's per-operation markers.

use std::path::{Path, PathBuf};

use crate::error::{InstallError, Result};
use crate::task::{InstallTask, Transaction};

/// The on-disk resume journal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Journal {
    /// The tasks that have not yet completed, in apply order.
    pub remaining: Vec<InstallTask>,
}

impl Journal {
    /// The journal file path under a state directory.
    pub fn path_in(state_dir: &Path) -> PathBuf {
        state_dir.join("transaction.journal")
    }

    /// Whether a journal exists under `state_dir`.
    pub fn exists_in(state_dir: &Path) -> bool {
        Self::path_in(state_dir).exists()
    }

    /// Begin a journal covering every task of `tx`.
    pub fn begin(tx: &Transaction) -> Self {
        Journal {
            remaining: tx.tasks.clone(),
        }
    }

    /// Load the journal from `state_dir`, if one exists.
    pub fn load(state_dir: &Path) -> Result<Option<Journal>> {
        let path = Self::path_in(state_dir);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let journal = rmp_serde::from_slice(&bytes).map_err(|e| InstallError::Journal {
                    path: path.clone(),
                    reason: e.to_string(),
                })?;
                Ok(Some(journal))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(InstallError::io(path, e)),
        }
    }

    /// Persist the journal to `state_dir`.
    pub fn save(&self, state_dir: &Path) -> Result<()> {
        let path = Self::path_in(state_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| InstallError::io(parent, e))?;
        }
        let bytes = rmp_serde::to_vec(self).map_err(|e| InstallError::Journal {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        moraine_common::fs::atomic_write(&path, &bytes).map_err(|e| InstallError::Journal {
            path: path.clone(),
            reason: e.to_string(),
        })
    }

    /// Drop the first remaining task (the one that just committed) and persist.
    pub fn complete_first(&mut self, state_dir: &Path) -> Result<()> {
        if !self.remaining.is_empty() {
            self.remaining.remove(0);
        }
        if self.remaining.is_empty() {
            Self::clear(state_dir)
        } else {
            self.save(state_dir)
        }
    }

    /// Remove the journal file from `state_dir`.
    pub fn clear(state_dir: &Path) -> Result<()> {
        let path = Self::path_in(state_dir);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(InstallError::io(path, e)),
        }
    }
}
