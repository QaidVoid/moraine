//! Transaction-loop tests against fake steps and a fake applier.
//!
//! These exercise ordering, failure isolation, locking, journal trimming,
//! interruption recovery, and resume without running real builds or touching the
//! live filesystem.

use std::cell::RefCell;
use std::collections::BTreeSet;

use moraine_install::engine::TransactionEngine;
use moraine_install::error::{InstallError, Result};
use moraine_install::step::{Applier, StepRunner};
use moraine_install::task::{InstallTask, Realized, Transaction};
use moraine_merge::state::PackageState;
use moraine_merge::{MergeOp, Operation, OperationOutcome, state::PostMergeReport};

/// A step runner that builds a merge op for every task and can be told to fail
/// on a particular cpv.
struct FakeRunner {
    fail_on: Option<String>,
    realized: RefCell<Vec<String>>,
}

impl FakeRunner {
    fn new() -> Self {
        FakeRunner {
            fail_on: None,
            realized: RefCell::new(Vec::new()),
        }
    }

    fn failing(cpv: &str) -> Self {
        FakeRunner {
            fail_on: Some(cpv.to_owned()),
            realized: RefCell::new(Vec::new()),
        }
    }
}

impl StepRunner for FakeRunner {
    fn realize(&self, task: &InstallTask) -> Result<Realized> {
        if self.fail_on.as_deref() == Some(task.cpv.as_str()) {
            return Err(InstallError::Realize {
                cpv: task.cpv.clone(),
                reason: "forced failure".to_owned(),
            });
        }
        self.realized.borrow_mut().push(task.cpv.clone());
        let op = MergeOp {
            image_dir: std::path::PathBuf::from("/nonexistent"),
            state: state_for(&task.cpv, &task.cp, &task.slot),
            replaces: task.replaces.clone(),
            in_world: task.in_world,
            elog: Vec::new(),
        };
        Ok(Realized::Apply(Operation::Merge(Box::new(op))))
    }
}

/// An applier that records applied operations and never touches disk.
struct FakeApplier {
    applied: RefCell<Vec<String>>,
    recovered: RefCell<bool>,
}

impl FakeApplier {
    fn new() -> Self {
        FakeApplier {
            applied: RefCell::new(Vec::new()),
            recovered: RefCell::new(false),
        }
    }
}

impl Applier for FakeApplier {
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

fn state_for(cpv: &str, cp: &str, slot: &str) -> PackageState {
    let (category, package) = cp.split_once('/').unwrap();
    let version = cpv.rsplit('-').next().unwrap_or("1");
    PackageState {
        cpv: cpv.to_owned(),
        category: category.to_owned(),
        package: package.to_owned(),
        version: version.to_owned(),
        eapi: "8".to_owned(),
        slot: slot.to_owned(),
        subslot: None,
        use_flags: Vec::new(),
        iuse: Vec::new(),
        depends: Default::default(),
        keywords: Vec::new(),
        license: String::new(),
        properties: String::new(),
        restrict: String::new(),
        repository: None,
        defined_phases: Vec::new(),
        build_time: None,
        chost: String::new(),
        provides: Vec::new(),
        requires: Vec::new(),
        environment: None,
        inherited: Vec::new(),
        features: Vec::new(),
        size: None,
        build_id: None,
        needed: Vec::new(),
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
fn applies_tasks_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new();
    let applier = FakeApplier::new();
    let engine = TransactionEngine::new(&runner, &applier, dir.path());
    let report = engine.run(&tx()).unwrap();
    assert_eq!(
        applier.applied.borrow().as_slice(),
        ["app/a-1", "app/b-2", "app/c-3"]
    );
    assert_eq!(report.applied.len(), 3);
    assert!(*applier.recovered.borrow());
}

#[test]
fn recover_runs_before_work() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new();
    let applier = FakeApplier::new();
    let engine = TransactionEngine::new(&runner, &applier, dir.path());
    engine.run(&tx()).unwrap();
    assert!(*applier.recovered.borrow());
}

#[test]
fn failure_stops_and_leaves_journal() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::failing("app/b-2");
    let applier = FakeApplier::new();
    let engine = TransactionEngine::new(&runner, &applier, dir.path());
    let err = engine.run(&tx()).unwrap_err();
    assert!(matches!(err, InstallError::Realize { .. }));
    // Only the first package committed; the failing one and the rest did not.
    assert_eq!(applier.applied.borrow().as_slice(), ["app/a-1"]);
    // The journal survives for resume, holding the unfinished tasks.
    assert!(moraine_install::engine::has_pending(dir.path()));
    let journal = moraine_install::Journal::load(dir.path()).unwrap().unwrap();
    assert_eq!(journal.remaining.len(), 2);
    assert_eq!(journal.remaining[0].cpv, "app/b-2");
}

#[test]
fn success_clears_journal() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new();
    let applier = FakeApplier::new();
    let engine = TransactionEngine::new(&runner, &applier, dir.path());
    engine.run(&tx()).unwrap();
    assert!(!moraine_install::engine::has_pending(dir.path()));
}

#[test]
fn resume_runs_remaining_only() {
    let dir = tempfile::tempdir().unwrap();
    // First run fails at b, leaving b and c in the journal.
    {
        let runner = FakeRunner::failing("app/b-2");
        let applier = FakeApplier::new();
        let engine = TransactionEngine::new(&runner, &applier, dir.path());
        engine.run(&tx()).unwrap_err();
    }
    // Resume with a healthy runner applies only b and c.
    let runner = FakeRunner::new();
    let applier = FakeApplier::new();
    let engine = TransactionEngine::new(&runner, &applier, dir.path());
    let report = engine.resume().unwrap();
    assert_eq!(applier.applied.borrow().as_slice(), ["app/b-2", "app/c-3"]);
    assert_eq!(report.applied.len(), 2);
    assert!(!moraine_install::engine::has_pending(dir.path()));
}

#[test]
fn resume_without_journal_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FakeRunner::new();
    let applier = FakeApplier::new();
    let engine = TransactionEngine::new(&runner, &applier, dir.path());
    let report = engine.resume().unwrap();
    assert!(report.applied.is_empty());
}

#[test]
fn world_additions_track_explicit_targets() {
    let dir = tempfile::tempdir().unwrap();
    let mut tasks = tx();
    tasks.tasks[0].in_world = true;
    let runner = FakeRunner::new();
    let applier = FakeApplier::new();
    let engine = TransactionEngine::new(&runner, &applier, dir.path());
    let report = engine.run(&tasks).unwrap();
    assert_eq!(report.world_additions, vec!["app/a".to_owned()]);
}

#[test]
fn second_transaction_blocks_on_lock() {
    let dir = tempfile::tempdir().unwrap();
    let _held = moraine_install::TransactionLock::acquire(dir.path()).unwrap();
    let runner = FakeRunner::new();
    let applier = FakeApplier::new();
    let engine = TransactionEngine::new(&runner, &applier, dir.path());
    let err = engine.run(&tx()).unwrap_err();
    assert!(matches!(err, InstallError::Locked { .. }));
    let _ = BTreeSet::<String>::new();
}
