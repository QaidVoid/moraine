//! The typed outcome of a backend synchronization.

/// Whether a backend performed an initial fetch or an update of an existing
/// tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncKind {
    /// The repository location did not exist and was fetched from scratch.
    Initial,
    /// The repository already existed and was updated in place.
    Update,
}

/// The result of synchronizing one repository through a backend.
///
/// This replaces the stock `(exitcode, updatecache_flg)` tuple with a typed
/// value: `changed` is the moral equivalent of `updatecache_flg` and drives the
/// post-sync metadata refresh, while `head` carries the new head revision when
/// the backend can report one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    /// Whether the operation was an initial fetch or an update.
    pub kind: SyncKind,
    /// Whether the repository tree changed as a result of the sync.
    pub changed: bool,
    /// The new head revision, when the backend can report one.
    pub head: Option<String>,
}

impl SyncOutcome {
    /// An outcome reporting no change and no known head.
    pub fn unchanged(kind: SyncKind) -> Self {
        Self {
            kind,
            changed: false,
            head: None,
        }
    }

    /// An outcome reporting a change with an optional head revision.
    pub fn changed(kind: SyncKind, head: Option<String>) -> Self {
        Self {
            kind,
            changed: true,
            head,
        }
    }
}
