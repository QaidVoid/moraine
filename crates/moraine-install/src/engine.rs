//! The transaction loop.
//!
//! [`TransactionEngine`] executes an ordered [`Transaction`] under the
//! transaction lock: it settles any interrupted operation, then for each task in
//! order it realizes the work (build or unpack) and applies one operation through
//! the merge engine, committing each package before the next is realized. A
//! failing task stops the transaction, leaving committed packages installed and
//! the failing package's live root untouched, and leaves the journal so the
//! remaining work can be resumed.

use std::collections::BTreeSet;
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
    /// The packages whose task failed under `--keep-going`, in failure order.
    /// Empty unless `--keep-going` is set, since a failure otherwise aborts.
    pub failures: Vec<PackageFailure>,
    /// The `category/package-version` of the dependents dropped under
    /// `--keep-going` because an in-transaction dependency failed.
    pub skipped: Vec<String>,
}

/// A package whose task failed and was dropped under `--keep-going`.
#[derive(Debug, Clone)]
pub struct PackageFailure {
    /// The `category/package-version` that failed.
    pub cpv: String,
    /// The failure reason.
    pub reason: String,
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
    keep_going: bool,
}

impl<'a, S: StepRunner, A: Applier> TransactionEngine<'a, S, A> {
    /// Build an engine over `state_dir` using `runner` and `applier`.
    pub fn new(runner: &'a S, applier: &'a A, state_dir: impl Into<PathBuf>) -> Self {
        TransactionEngine {
            runner,
            applier,
            state_dir: state_dir.into(),
            keep_going: false,
        }
    }

    /// Set whether a failed task drops only the failed package and its
    /// in-transaction dependents and continues, instead of aborting the
    /// transaction. Mirrors `emerge --keep-going`.
    pub fn with_keep_going(mut self, keep_going: bool) -> Self {
        self.keep_going = keep_going;
        self
    }

    /// Run a fresh transaction: write the journal and apply every task.
    pub fn run(&self, tx: &Transaction) -> Result<TransactionReport> {
        let _lock = TransactionLock::acquire(&self.state_dir)?;
        self.applier.recover()?;
        // Upfront pkg_pretend pass: validate every source merge task in
        // transaction order before any fetch, build, or merge, so a later task's
        // pretend failure never leaves earlier packages partially applied to the
        // live root, mirroring Portage's `Scheduler._run_pkg_pretend`.
        for task in &tx.tasks {
            if task.kind == TaskKind::Merge {
                self.runner.pretend(task)?;
            }
        }
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
    ///
    /// Without `keep_going` a failing task aborts, leaving the journal so the
    /// remaining work can be resumed. With `keep_going` a failing task is
    /// recorded and dropped, and any later task that depends on a dropped package
    /// is skipped and itself dropped, so the drop propagates forward through the
    /// topologically ordered list while the independent remainder still merges.
    fn drive(&self, journal: &mut Journal) -> Result<TransactionReport> {
        let mut report = TransactionReport::default();
        let mut dropped: BTreeSet<String> = BTreeSet::new();
        while let Some(task) = journal.remaining.first().cloned() {
            if self.keep_going && task.requires.iter().any(|cp| dropped.contains(cp)) {
                report.skipped.push(task.cpv.clone());
                dropped.insert(task.cp.clone());
                journal.complete_first(&self.state_dir)?;
                continue;
            }
            let applied = match self.run_task(&task) {
                Ok(applied) => applied,
                Err(error) if self.keep_going => {
                    report.failures.push(PackageFailure {
                        cpv: task.cpv.clone(),
                        reason: error.to_string(),
                    });
                    dropped.insert(task.cp.clone());
                    journal.complete_first(&self.state_dir)?;
                    continue;
                }
                Err(error) => return Err(error),
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::InstallTask;
    use moraine_merge::state::PostMergeReport;
    use std::cell::RefCell;

    /// A step runner that records the order of `pretend`/`realize` calls and can
    /// be told to fail one task's pretend.
    struct PretendRunner {
        fail_pretend_on: Option<String>,
        fail_realize_on: Option<String>,
        pretended: RefCell<Vec<String>>,
        realized: RefCell<Vec<String>>,
    }

    impl PretendRunner {
        fn new() -> Self {
            PretendRunner {
                fail_pretend_on: None,
                fail_realize_on: None,
                pretended: RefCell::new(Vec::new()),
                realized: RefCell::new(Vec::new()),
            }
        }

        fn failing(cpv: &str) -> Self {
            PretendRunner {
                fail_pretend_on: Some(cpv.to_owned()),
                ..PretendRunner::new()
            }
        }

        fn failing_realize(cpv: &str) -> Self {
            PretendRunner {
                fail_realize_on: Some(cpv.to_owned()),
                ..PretendRunner::new()
            }
        }
    }

    impl StepRunner for PretendRunner {
        fn realize(&self, task: &InstallTask) -> Result<Realized> {
            if self.fail_realize_on.as_deref() == Some(task.cpv.as_str()) {
                return Err(InstallError::Realize {
                    cpv: task.cpv.clone(),
                    reason: "build failed".to_owned(),
                });
            }
            self.realized.borrow_mut().push(task.cpv.clone());
            Ok(Realized::PackagedOnly)
        }

        fn pretend(&self, task: &InstallTask) -> Result<()> {
            self.pretended.borrow_mut().push(task.cpv.clone());
            if self.fail_pretend_on.as_deref() == Some(task.cpv.as_str()) {
                return Err(InstallError::Realize {
                    cpv: task.cpv.clone(),
                    reason: "pkg_pretend failed".to_owned(),
                });
            }
            Ok(())
        }
    }

    /// An applier that records applications and never touches disk.
    struct RecordingApplier {
        recovered: RefCell<bool>,
        applied: RefCell<Vec<String>>,
    }

    impl RecordingApplier {
        fn new() -> Self {
            RecordingApplier {
                recovered: RefCell::new(false),
                applied: RefCell::new(Vec::new()),
            }
        }
    }

    impl Applier for RecordingApplier {
        fn recover(&self) -> Result<()> {
            *self.recovered.borrow_mut() = true;
            Ok(())
        }

        fn apply(&self, op: &Operation) -> Result<OperationOutcome> {
            let cpv = op.label().to_owned();
            self.applied.borrow_mut().push(cpv.clone());
            Ok(OperationOutcome {
                cpv,
                merged: matches!(op, Operation::Merge(_)),
                counter: Some(1),
                report: PostMergeReport::default(),
                preserved: Vec::new(),
                reconciled: Vec::new(),
            })
        }
    }

    fn tx() -> Transaction {
        Transaction::new(vec![
            InstallTask::merge("app/a-1", "app/a", "0"),
            InstallTask::merge("app/b-2", "app/b", "0"),
            InstallTask::merge("app/c-3", "app/c", "0"),
        ])
    }

    #[test]
    fn failing_pretend_aborts_before_any_realize() {
        let dir = tempfile::tempdir().unwrap();
        let runner = PretendRunner::failing("app/c-3");
        let applier = RecordingApplier::new();
        let engine = TransactionEngine::new(&runner, &applier, dir.path());
        let err = engine.run(&tx());
        assert!(err.is_err(), "a failing pretend must abort the transaction");
        // The abort happened in the upfront pass, before anything was realized or
        // applied.
        assert!(
            runner.realized.borrow().is_empty(),
            "no task may be realized when an earlier pretend fails"
        );
        assert!(applier.applied.borrow().is_empty());
        // The pretend pass reached the failing task in transaction order.
        assert_eq!(
            runner.pretended.borrow().as_slice(),
            ["app/a-1", "app/b-2", "app/c-3"]
        );
    }

    #[test]
    fn pretend_pass_precedes_the_merge_loop() {
        let dir = tempfile::tempdir().unwrap();
        let runner = PretendRunner::new();
        let applier = RecordingApplier::new();
        let engine = TransactionEngine::new(&runner, &applier, dir.path());
        engine.run(&tx()).unwrap();
        // Every merge task was validated upfront, then realized.
        assert_eq!(
            runner.pretended.borrow().as_slice(),
            ["app/a-1", "app/b-2", "app/c-3"]
        );
        assert_eq!(
            runner.realized.borrow().as_slice(),
            ["app/a-1", "app/b-2", "app/c-3"]
        );
    }

    /// Build the three-task transaction where `app/c` depends on the failing
    /// `app/b` and `app/a` is independent.
    fn keep_going_tx() -> Transaction {
        let a = InstallTask::merge("app/a-1", "app/a", "0");
        let b = InstallTask::merge("app/b-2", "app/b", "0");
        let mut c = InstallTask::merge("app/c-3", "app/c", "0");
        c.requires = vec!["app/b".to_owned()];
        Transaction::new(vec![a, b, c])
    }

    #[test]
    fn failure_without_keep_going_aborts_and_keeps_journal() {
        let dir = tempfile::tempdir().unwrap();
        let runner = PretendRunner::failing_realize("app/b-2");
        let applier = RecordingApplier::new();
        let engine = TransactionEngine::new(&runner, &applier, dir.path());
        assert!(engine.run(&keep_going_tx()).is_err());
        // The run stopped at the failing task: only the independent earlier task
        // was realized, and the journal remains for a resume.
        assert_eq!(runner.realized.borrow().as_slice(), ["app/a-1"]);
        assert!(has_pending(dir.path()));
    }

    #[test]
    fn keep_going_drops_failure_and_dependents_and_continues() {
        let dir = tempfile::tempdir().unwrap();
        let runner = PretendRunner::failing_realize("app/b-2");
        let applier = RecordingApplier::new();
        let engine = TransactionEngine::new(&runner, &applier, dir.path()).with_keep_going(true);
        let report = engine.run(&keep_going_tx()).unwrap();
        // The independent task still completed.
        assert_eq!(runner.realized.borrow().as_slice(), ["app/a-1"]);
        assert_eq!(report.applied.len(), 1);
        assert_eq!(report.applied[0].cpv, "app/a-1");
        // The failed package and its dependent are recorded.
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].cpv, "app/b-2");
        assert_eq!(report.skipped, ["app/c-3"]);
        // The journal is cleared at the end of a keep-going run.
        assert!(!has_pending(dir.path()));
    }
}
