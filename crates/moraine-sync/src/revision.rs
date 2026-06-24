//! Per-repository revision history.
//!
//! After a successful sync the engine records the repository's head revision in
//! a bounded, descending-time-order history. A repository whose backend cannot
//! report a head is recorded with no revision rather than failing the sync. The
//! history is held in memory and serialized to a simple line-based file under the
//! rewrite's state directory, kept separate from the stock `repo_revisions` file.

use std::collections::BTreeMap;
use std::path::Path;

use crate::error::SyncError;

/// The number of recent revisions retained per repository, matching the stock
/// limit.
pub const HISTORY_LIMIT: usize = 25;

/// A bounded per-repository revision history in descending time order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RevisionHistory {
    histories: BTreeMap<String, Vec<String>>,
}

impl RevisionHistory {
    /// An empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a head revision for `repo`. A `None` revision records the
    /// repository with no revision and does not modify any existing history. A
    /// revision equal to the most recent recorded one is not duplicated. A new
    /// revision is prepended and the history is bounded to [`HISTORY_LIMIT`].
    pub fn record(&mut self, repo: &str, head: Option<&str>) {
        let entry = self.histories.entry(repo.to_owned()).or_default();
        let Some(rev) = head else {
            return;
        };
        if entry.first().map(String::as_str) == Some(rev) {
            return;
        }
        entry.insert(0, rev.to_owned());
        entry.truncate(HISTORY_LIMIT);
    }

    /// The recorded revisions for `repo`, most recent first.
    pub fn revisions(&self, repo: &str) -> &[String] {
        self.histories.get(repo).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The most recent recorded revision for `repo`, when any.
    pub fn latest(&self, repo: &str) -> Option<&str> {
        self.revisions(repo).first().map(String::as_str)
    }

    /// Serialize the history to a line-based representation: one `repo<TAB>rev`
    /// line per revision in stored order.
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        for (repo, revs) in &self.histories {
            for rev in revs {
                out.push_str(repo);
                out.push('\t');
                out.push_str(rev);
                out.push('\n');
            }
        }
        out
    }

    /// Parse a history from its serialized representation.
    pub fn parse(text: &str) -> Self {
        let mut history = Self::new();
        for line in text.lines() {
            if let Some((repo, rev)) = line.split_once('\t') {
                history
                    .histories
                    .entry(repo.to_owned())
                    .or_default()
                    .push(rev.to_owned());
            }
        }
        history
    }

    /// Load a history from `path`, returning an empty history when the file is
    /// absent.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SyncError> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new()),
            Err(source) => Err(SyncError::Io {
                path: path.to_path_buf(),
                reason: source.to_string(),
            }),
        }
    }

    /// Atomically write the history to `path`.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), SyncError> {
        let path = path.as_ref();
        moraine_common::fs::atomic_write(path, self.serialize().as_bytes()).map_err(|source| {
            SyncError::Io {
                path: path.to_path_buf(),
                reason: source.to_string(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_bounds_history() {
        let mut h = RevisionHistory::new();
        for i in 0..(HISTORY_LIMIT + 5) {
            h.record("gentoo", Some(&format!("rev{i}")));
        }
        let revs = h.revisions("gentoo");
        assert_eq!(revs.len(), HISTORY_LIMIT);
        // Most recent first.
        assert_eq!(revs[0], format!("rev{}", HISTORY_LIMIT + 4));
    }

    #[test]
    fn duplicate_head_not_recorded() {
        let mut h = RevisionHistory::new();
        h.record("r", Some("a"));
        h.record("r", Some("a"));
        assert_eq!(h.revisions("r"), &["a".to_owned()]);
    }

    #[test]
    fn missing_head_records_no_revision() {
        let mut h = RevisionHistory::new();
        h.record("r", None);
        assert!(h.revisions("r").is_empty());
        assert_eq!(h.latest("r"), None);
    }

    #[test]
    fn round_trips_through_serialization() {
        let mut h = RevisionHistory::new();
        h.record("a", Some("a1"));
        h.record("a", Some("a2"));
        h.record("b", Some("b1"));
        let parsed = RevisionHistory::parse(&h.serialize());
        assert_eq!(parsed, h);
    }
}
