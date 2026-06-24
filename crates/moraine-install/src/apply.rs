//! The real merge-engine applier.
//!
//! [`EngineApplier`] wraps [`moraine_merge::MergeEngine`] to satisfy the
//! orchestrator's [`Applier`] trait. Each call applies a single operation under
//! the merge engine's own installed-store lock, so a package is durably recorded
//! before the orchestrator realizes the next one. The outer transaction lock
//! guards the whole sequence; this inner lock keeps each commit atomic.

use moraine_merge::{MergeContext, MergeEngine, Operation, OperationOutcome};

use crate::error::{InstallError, Result};
use crate::step::Applier;

/// An [`Applier`] backed by the live-filesystem merge engine.
pub struct EngineApplier {
    engine: MergeEngine,
}

impl EngineApplier {
    /// Build an applier for `ctx`.
    pub fn new(ctx: MergeContext) -> Self {
        EngineApplier {
            engine: MergeEngine::new(ctx),
        }
    }
}

impl Applier for EngineApplier {
    fn recover(&self) -> Result<()> {
        self.engine
            .recover()
            .map_err(|source| InstallError::Merge {
                cpv: String::new(),
                source,
            })?;
        Ok(())
    }

    fn apply(&self, op: &Operation) -> Result<OperationOutcome> {
        let cpv = op.label().to_owned();
        let mut outcomes = self
            .engine
            .apply(std::slice::from_ref(op))
            .map_err(|source| InstallError::Merge {
                cpv: cpv.clone(),
                source,
            })?;
        outcomes.pop().ok_or_else(|| InstallError::Realize {
            cpv,
            reason: "merge engine returned no outcome".to_owned(),
        })
    }
}
