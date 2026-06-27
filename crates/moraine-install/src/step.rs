//! The injectable steps of a transaction.
//!
//! The orchestrator's loop is generic over two traits so the ordering, locking,
//! journal, and resume logic can be tested against fakes without running real
//! builds or touching the live filesystem. [`StepRunner`] realizes a merge task
//! into work for the merge engine (fetching, building, or unpacking as needed),
//! and [`Applier`] applies one operation through the merge engine.

use moraine_merge::{Operation, OperationOutcome};

use crate::error::Result;
use crate::task::{InstallTask, Realized};

/// Realizes a merge task into an installable operation.
///
/// Implementations perform the fetch, build, or binary-package unpack required
/// to produce an image and return the [`Operation`] the merge engine applies, or
/// [`Realized::PackagedOnly`] when the task only produced a binary package.
pub trait StepRunner {
    /// Realize one merge task.
    fn realize(&self, task: &InstallTask) -> Result<Realized>;

    /// Validate one merge task's `pkg_pretend` before the merge loop.
    ///
    /// The transaction engine runs this upfront for every source merge task, so a
    /// failing `pkg_pretend` aborts the whole transaction before anything is
    /// fetched, built, or merged. The default is a no-op, leaving binary-package
    /// runners and test fakes unaffected; the from-source runner overrides it.
    fn pretend(&self, task: &InstallTask) -> Result<()> {
        let _ = task;
        Ok(())
    }
}

/// Applies operations through the live-filesystem merge engine.
///
/// Implementations own the dangerous write surface. The orchestrator calls
/// [`recover`](Applier::recover) once at the start of a transaction and
/// [`apply`](Applier::apply) once per task, in order.
pub trait Applier {
    /// Settle any interrupted operation found at startup.
    fn recover(&self) -> Result<()>;

    /// Apply a single operation and return its outcome.
    fn apply(&self, op: &Operation) -> Result<OperationOutcome>;
}
