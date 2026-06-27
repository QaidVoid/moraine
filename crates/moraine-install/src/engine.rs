//! The transaction loop.
//!
//! [`TransactionEngine`] executes an ordered [`Transaction`] under the
//! transaction lock: it settles any interrupted operation, then for each task in
//! order it realizes the work (build or unpack) and applies one operation through
//! the merge engine, committing each package before the next is realized. A
//! failing task stops the transaction, leaving committed packages installed and
//! the failing package's live root untouched, and leaves the journal so the
//! remaining work can be resumed.

use std::path::{Path, PathBuf};

use moraine_merge::{Operation, OperationOutcome, UnmergeOp};

use crate::error::{InstallError, Result};
use crate::journal::Journal;
use crate::lock::TransactionLock;
use crate::step::{Applier, StepRunner};
use crate::task::{InstallTask, Realized, TaskKind, Transaction};

/// The recorded result of one applied task.
#[derive(Debug, Clone)]
pub struct TaskOutcome {
    /// The `category/package-version` the task concerned.
    pub cpv: String,
    /// The merge-engine outcome, absent for a package-only build.
    pub outcome: Option<OperationOutcome>,
}

/// The result of running a transaction to completion.
#[derive(Debug, Clone, Default)]
pub struct TransactionReport {
    /// The per-task outcomes, in apply order.
    pub applied: Vec<TaskOutcome>,
    /// The resolved world atoms of merged packages that should join the world
    /// set, slot-qualified or repo-qualified when the request was that precise.
    pub world_additions: Vec<String>,
    /// Pending CONFIG_PROTECT variant paths left by the transaction.
    pub config_updates: Vec<String>,
    /// The build-time elog messages aggregated across the transaction, each
    /// tagged with the package that produced it.
    pub elog: Vec<PackageElog>,
}

/// The build-time elog of one merged package, for end-of-run dispatch.
#[derive(Debug, Clone)]
pub struct PackageElog {
    /// The `category/package-version` that produced the messages.
    pub cpv: String,
    /// The elog records carried through the merge.
    pub messages: Vec<moraine_merge::ElogRecord>,
}

/// Executes transactions against an injected step runner and applier.
pub struct TransactionEngine<'a, S: StepRunner, A: Applier> {
    runner: &'a S,
    applier: &'a A,
    state_dir: PathBuf,
}

impl<'a, S: StepRunner, A: Applier> TransactionEngine<'a, S, A> {
    /// Build an engine over `state_dir` using `runner` and `applier`.
    pub fn new(runner: &'a S, applier: &'a A, state_dir: impl Into<PathBuf>) -> Self {
        TransactionEngine {
            runner,
            applier,
            state_dir: state_dir.into(),
        }
    }

    /// Run a fresh transaction: write the journal and apply every task.
    pub fn run(&self, tx: &Transaction) -> Result<TransactionReport> {
        let _lock = TransactionLock::acquire(&self.state_dir)?;
        self.applier.recover()?;
        let mut journal = Journal::begin(tx);
        journal.save(&self.state_dir)?;
        self.drive(&mut journal)
    }

    /// Resume the most recent transaction from its journal, applying the tasks
    /// that did not complete. Returns an empty report when no journal exists.
    pub fn resume(&self) -> Result<TransactionReport> {
        let _lock = TransactionLock::acquire(&self.state_dir)?;
        self.applier.recover()?;
        let Some(mut journal) = Journal::load(&self.state_dir)? else {
            return Ok(TransactionReport::default());
        };
        self.drive(&mut journal)
    }

    /// Apply the remaining tasks of `journal` in order, trimming as each commits.
    fn drive(&self, journal: &mut Journal) -> Result<TransactionReport> {
        let mut report = TransactionReport::default();
        while let Some(task) = journal.remaining.first().cloned() {
            let applied = self.run_task(&task)?;
            if let Some(outcome) = &applied.outcome {
                report
                    .config_updates
                    .extend(outcome.report.config_updates.iter().cloned());
                if !outcome.report.elog.is_empty() {
                    report.elog.push(PackageElog {
                        cpv: task.cpv.clone(),
                        messages: outcome.report.elog.clone(),
                    });
                }
                if let Some(atom) = &task.world_atom {
                    report.world_additions.push(atom.clone());
                }
            }
            report.applied.push(applied);
            journal.complete_first(&self.state_dir)?;
        }
        Ok(report)
    }

    /// Realize and apply one task.
    fn run_task(&self, task: &InstallTask) -> Result<TaskOutcome> {
        match task.kind {
            TaskKind::Uninstall => {
                let op = Operation::Unmerge(UnmergeOp {
                    cpv: task.cpv.clone(),
                    replaced: task.replaces.is_some(),
                });
                let outcome = self
                    .applier
                    .apply(&op)
                    .map_err(|e| merge_error(&task.cpv, e))?;
                Ok(TaskOutcome {
                    cpv: task.cpv.clone(),
                    outcome: Some(outcome),
                })
            }
            TaskKind::Merge => match self.runner.realize(task)? {
                Realized::Apply(op) => {
                    let outcome = self
                        .applier
                        .apply(&op)
                        .map_err(|e| merge_error(&task.cpv, e))?;
                    Ok(TaskOutcome {
                        cpv: task.cpv.clone(),
                        outcome: Some(outcome),
                    })
                }
                Realized::PackagedOnly => Ok(TaskOutcome {
                    cpv: task.cpv.clone(),
                    outcome: None,
                }),
            },
        }
    }
}

/// Whether a transaction journal is pending under `state_dir`.
pub fn has_pending(state_dir: &Path) -> bool {
    Journal::exists_in(state_dir)
}

/// Re-tag an applier error with the package it concerned, preserving an already
/// tagged error.
fn merge_error(cpv: &str, error: InstallError) -> InstallError {
    match error {
        InstallError::Merge { source, .. } => InstallError::Merge {
            cpv: cpv.to_owned(),
            source,
        },
        other => other,
    }
}
