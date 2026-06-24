//! The transaction input types.
//!
//! A [`Transaction`] is an ordered list of [`InstallTask`]s produced from the
//! resolver's task list. Each task carries everything the orchestrator needs to
//! realize it (build from source or install a binary package) and to record the
//! resulting world and journal state, without reaching back into the resolver.

use moraine_merge::Operation;
use serde::{Deserialize, Serialize};

/// Whether a task installs a package or removes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskKind {
    /// Build or unpack and merge a package into the live root.
    Merge,
    /// Unmerge an installed package from the live root.
    Uninstall,
}

/// Where a merge task's image comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    /// Build from source through the build engine.
    Source,
    /// Install from a binary package.
    Binary,
}

/// One unit of work in a transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallTask {
    /// The `category/package-version` this task concerns.
    pub cpv: String,
    /// The `category/package` of the task.
    pub cp: String,
    /// The resolved slot.
    pub slot: String,
    /// Whether this merges or uninstalls.
    pub kind: TaskKind,
    /// For a merge, where the image comes from.
    pub source: SourceKind,
    /// Whether this package was explicitly requested and so joins `@world`.
    pub in_world: bool,
    /// The `category/package-version` of a prior version replaced in the same
    /// slot, if any.
    pub replaces: Option<String>,
}

impl InstallTask {
    /// A merge task built from source for `cpv` in `slot`.
    pub fn merge(cpv: impl Into<String>, cp: impl Into<String>, slot: impl Into<String>) -> Self {
        InstallTask {
            cpv: cpv.into(),
            cp: cp.into(),
            slot: slot.into(),
            kind: TaskKind::Merge,
            source: SourceKind::Source,
            in_world: false,
            replaces: None,
        }
    }

    /// An uninstall task for `cpv` in `slot`.
    pub fn uninstall(
        cpv: impl Into<String>,
        cp: impl Into<String>,
        slot: impl Into<String>,
    ) -> Self {
        InstallTask {
            cpv: cpv.into(),
            cp: cp.into(),
            slot: slot.into(),
            kind: TaskKind::Uninstall,
            source: SourceKind::Source,
            in_world: false,
            replaces: None,
        }
    }
}

/// An ordered list of tasks to apply as a single transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    /// The tasks, in apply order.
    pub tasks: Vec<InstallTask>,
}

impl Transaction {
    /// A transaction over the given tasks.
    pub fn new(tasks: Vec<InstallTask>) -> Self {
        Transaction { tasks }
    }

    /// Whether the transaction has no work.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

/// The outcome of realizing one merge task into work for the merge engine.
#[derive(Debug)]
pub enum Realized {
    /// An operation to apply through the merge engine.
    Apply(Operation),
    /// The task produced a binary package only and must not be merged.
    PackagedOnly,
}
