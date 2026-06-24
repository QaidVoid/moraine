//! Corpus harness for importing a real Gentoo data snapshot.
//!
//! A corpus is a copy of a real system's data placed in the git-ignored
//! `corpus/` directory and used to compare Moraine's results and timings
//! against stock Portage. During bootstrap this only validates and summarizes
//! the corpus layout; the real metadata and installed-store importers arrive
//! with their respective phases. See `docs/corpus.md` for how to populate it.

use std::path::{Path, PathBuf};

use miette::Diagnostic;
use thiserror::Error;

/// Errors from corpus operations.
#[derive(Debug, Error, Diagnostic)]
pub enum CorpusError {
    /// The corpus root does not exist.
    #[error("corpus root not found at `{path}`")]
    #[diagnostic(
        code(moraine::corpus::missing),
        help("populate it as described in docs/corpus.md")
    )]
    Missing {
        /// The path that was expected to hold the corpus.
        path: PathBuf,
    },

    /// An I/O error while reading the corpus.
    #[error("failed to read corpus at `{path}`")]
    #[diagnostic(code(moraine::corpus::io))]
    Io {
        /// The path being read when the error occurred.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// A summary of a corpus directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusSummary {
    /// Number of top-level entries found under the corpus root.
    pub entries: usize,
}

/// Validate and summarize the corpus at `root`.
///
/// This is the bootstrap entry point for the corpus harness: it confirms the
/// directory exists and counts its top-level entries. Later phases replace the
/// body with the real metadata and installed-store importers.
pub fn import_corpus(root: impl AsRef<Path>) -> Result<CorpusSummary, CorpusError> {
    let root = root.as_ref();
    if !root.exists() {
        return Err(CorpusError::Missing {
            path: root.to_path_buf(),
        });
    }
    let read_dir = std::fs::read_dir(root).map_err(|source| CorpusError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let mut entries = 0usize;
    for entry in read_dir {
        entry.map_err(|source| CorpusError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        entries += 1;
    }
    tracing::info!(entries, "summarized corpus");
    Ok(CorpusSummary { entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_corpus_reports_missing() {
        let err = import_corpus("/nonexistent/moraine/corpus").unwrap_err();
        assert!(matches!(err, CorpusError::Missing { .. }));
    }

    #[test]
    fn counts_top_level_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("dev-lang")).unwrap();
        std::fs::create_dir(dir.path().join("sys-apps")).unwrap();
        let summary = import_corpus(dir.path()).unwrap();
        assert_eq!(summary, CorpusSummary { entries: 2 });
    }
}
