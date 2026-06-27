//! The operation type consumed from the resolver task list.
//!
//! The resolver produces an ordered task list that already encodes merge versus
//! unmerge and the replacement relationship within a slot. The merge engine
//! consumes that order verbatim and does not re-derive it.

use std::path::PathBuf;

use crate::state::PackageState;

/// One operation in the ordered task list: a merge or an unmerge.
#[derive(Debug, Clone)]
pub enum Operation {
    /// Merge a built image into the live root and record the package.
    Merge(Box<MergeOp>),
    /// Unmerge an installed package from the live root.
    Unmerge(UnmergeOp),
}

impl Operation {
    /// A short `category/package-version` label for tracing and markers.
    pub fn label(&self) -> &str {
        match self {
            Operation::Merge(m) => &m.state.cpv,
            Operation::Unmerge(u) => &u.cpv,
        }
    }
}

/// A merge operation: the image to install and the state to record.
#[derive(Debug, Clone)]
pub struct MergeOp {
    /// The built image directory (`D`) to install from.
    pub image_dir: PathBuf,
    /// The installed-store record to write at commit, minus the counter.
    pub state: PackageState,
    /// The `category/package-version` of a prior version being replaced in the
    /// same slot, if this merge replaces one.
    pub replaces: Option<String>,
    /// The resolved world atom to record when the package joins `@world`, or
    /// `None` when it does not. Carries a slot-qualified (`cp:slot`) or
    /// repo-qualified (`::repo`) atom when the request was that precise.
    pub world_atom: Option<String>,
    /// The build-time elog messages to carry into the post-merge report.
    pub elog: Vec<crate::state::ElogRecord>,
    /// The ebuild source bytes to copy into the dbdir as `<PF>.ebuild`, when
    /// available.
    pub ebuild: Option<Vec<u8>>,
}

/// An unmerge operation: the package to remove from the live root.
#[derive(Debug, Clone)]
pub struct UnmergeOp {
    /// The `category/package-version` of the package to unmerge.
    pub cpv: String,
    /// Whether this is part of a same-slot replacement (so a later merge owns the
    /// slot) and should not be removed from `@world`.
    pub replaced: bool,
}
