//! The merge engine: lock, order, apply, record, reconcile, recover.
//!
//! [`MergeEngine`] is the single entry point. It acquires the installed-store
//! lock, applies operations strictly in task-list order one at a time, records
//! the installed state as the commit point of each merge, updates `@world` after
//! commit, bumps the global counter per installed instance, reconciles the
//! preserved-libs registry after every operation, and recovers an interrupted
//! operation found at startup.

use std::path::{Path, PathBuf};

use moraine_vdb::store::{Store, StorePaths};

use crate::error::{IoResultExt as _, MergeError};
use crate::plan::{MergeOp, Operation, UnmergeOp};
use crate::preserve::{self, PreservedLibs};
use crate::recovery::{self, MarkerKind};
use crate::state::{self, PostMergeReport};
use crate::{MergeContext, merge, unmerge};

/// The recorded outcome of applying one operation.
#[derive(Debug, Clone)]
pub struct OperationOutcome {
    /// The `category/package-version` the operation concerned.
    pub cpv: String,
    /// Whether it was a merge (else an unmerge).
    pub merged: bool,
    /// The counter stamped on a merged record, if a merge.
    pub counter: Option<u64>,
    /// The post-merge report (elog, news, config updates).
    pub report: PostMergeReport,
    /// The preserved-library paths kept alive by this operation.
    pub preserved: Vec<String>,
    /// The preserved-library paths dropped by reconciliation after this
    /// operation.
    pub reconciled: Vec<String>,
}

/// A held installed-store lock, released on drop.
struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    fn acquire(path: &Path) -> Result<Self, MergeError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_path(parent)?;
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|source| MergeError::Lock {
                path: path.to_path_buf(),
                source,
            })?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The merge engine bound to a live-system context.
pub struct MergeEngine {
    ctx: MergeContext,
}

impl MergeEngine {
    /// Create an engine for `ctx`.
    pub fn new(ctx: MergeContext) -> Self {
        Self { ctx }
    }

    /// The store paths under the configured vdb directory.
    fn store_paths(&self) -> StorePaths {
        StorePaths::in_dir(&self.ctx.vdb_dir)
    }

    /// Load the global counter, falling back to the store counter when no global
    /// counter file exists yet.
    fn load_counter(&self, store: &Store) -> u64 {
        std::fs::read_to_string(self.ctx.counter_file())
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or_else(|| store.counter())
    }

    /// Persist the global counter atomically.
    fn save_counter(&self, value: u64) -> Result<(), MergeError> {
        moraine_common::fs::atomic_write(self.ctx.counter_file(), value.to_string().as_bytes())?;
        Ok(())
    }

    /// Load the preserved-libs registry, rebuilding it from installed soname data
    /// when it fails validation.
    fn load_registry(&self, store: &Store) -> Result<PreservedLibs, MergeError> {
        match PreservedLibs::load(&self.ctx.registry_file()) {
            Ok(reg) => Ok(reg),
            Err(MergeError::Registry { .. }) => {
                tracing::warn!("preserved-libs registry corrupt; rebuilding from soname data");
                Ok(self.rebuild_registry(store))
            }
            Err(e) => Err(e),
        }
    }

    /// Rebuild the registry from installed soname data: any installed file whose
    /// basename is a soname still required by another package is a preserved lib.
    fn rebuild_registry(&self, store: &Store) -> PreservedLibs {
        let interner = store.interner();
        let mut reg = PreservedLibs::new();
        for record in store.records() {
            let cpv = record.cpv(interner);
            let provided: Vec<String> = record
                .provides
                .entries
                .iter()
                .filter_map(|e| interner.resolve(e.soname).map(|s| s.to_string()))
                .collect();
            for entry in record.contents.iter() {
                let base = entry.path.rsplit('/').next().unwrap_or(&entry.path);
                if provided.iter().any(|s| s == base)
                    && preserve::soname_still_needed(store, interner, base, Some(&cpv))
                {
                    reg.insert(crate::preserve::PreservedEntry {
                        cpv: cpv.clone(),
                        soname: base.to_string(),
                        path: entry.path.clone(),
                    });
                }
            }
        }
        reg
    }

    /// Apply an ordered task list of operations, returning the per-operation
    /// outcomes. Recovers any interrupted operation found at startup first.
    ///
    /// The whole sequence runs under the installed-store lock, one operation at a
    /// time, in the given order.
    pub fn apply(&self, ops: &[Operation]) -> Result<Vec<OperationOutcome>, MergeError> {
        let _lock = LockGuard::acquire(&self.ctx.lock_file())?;

        self.recover()?;

        let mut store = Store::load(self.store_paths())?;
        let mut counter = self.load_counter(&store);
        let mut registry = self.load_registry(&store)?;
        let mut outcomes = Vec::new();

        for op in ops {
            let outcome = match op {
                Operation::Merge(m) => {
                    self.apply_merge(&mut store, &mut counter, &mut registry, m)?
                }
                Operation::Unmerge(u) => self.apply_unmerge(&mut store, &mut registry, u)?,
            };
            outcomes.push(outcome);
        }

        // Compact so the primary store reflects the applied journal.
        store.compact()?;
        self.save_counter(counter)?;
        registry.save(&self.ctx.registry_file())?;

        Ok(outcomes)
    }

    /// Apply a single merge: place files, record state (the commit point), update
    /// world, bump the counter, then reconcile.
    fn apply_merge(
        &self,
        store: &mut Store,
        counter: &mut u64,
        registry: &mut PreservedLibs,
        op: &MergeOp,
    ) -> Result<OperationOutcome, MergeError> {
        recovery::write_marker(&self.ctx.marker_dir(), MarkerKind::Merge, &op.state.cpv)?;

        let interner = store.interner().clone();
        let result = merge::place_image(&self.ctx, store, &interner, registry, op)?;

        // Build CONTENTS, including any preserved-library paths, and record the
        // installed state. Visibility of the record is the commit point.
        let entries = merge::entries_with_preserved(result.entries, &result.preserved);
        let contents = state::contents_from(entries);
        *counter += 1;
        let stamped = *counter;
        let record = op.state.clone().into_record(&interner, contents, stamped)?;
        store.add(record)?;

        // A same-slot replacement supersedes the prior version's installed
        // record now that the new version is the visible owner of the slot.
        if let Some(prior) = op.replaces.as_deref()
            && let Some((category, package, version)) = split_cpv(store, &interner, prior)
        {
            store.remove(category, package, &version)?;
        }

        // World update happens only after the record is committed.
        if op.in_world {
            self.add_to_world(&op.state.category, &op.state.package)?;
        }

        // Commit point reached: clear the in-progress marker.
        recovery::clear_marker(&self.ctx.marker_dir())?;

        // Reconcile preserved libs after the operation.
        let reconciled = self.reconcile(store, registry)?;

        let report = PostMergeReport {
            elog: vec![format!("merged {}", op.state.cpv)],
            news_marked: vec![op.state.cpv.clone()],
            config_updates: result.config_updates,
        };

        Ok(OperationOutcome {
            cpv: op.state.cpv.clone(),
            merged: true,
            counter: Some(stamped),
            report,
            preserved: result.preserved.into_iter().map(|p| p.path).collect(),
            reconciled,
        })
    }

    /// Apply a single unmerge: remove CONTENTS safely, drop from the store and
    /// world, then reconcile.
    fn apply_unmerge(
        &self,
        store: &mut Store,
        registry: &mut PreservedLibs,
        op: &UnmergeOp,
    ) -> Result<OperationOutcome, MergeError> {
        recovery::write_marker(&self.ctx.marker_dir(), MarkerKind::Unmerge, &op.cpv)?;

        let interner = store.interner().clone();
        let _ = unmerge::unmerge(&self.ctx, store, &interner, &op.cpv)?;

        // Remove the record and, for an explicit package not being replaced, drop
        // it from the world file.
        if let Some((category, package, version)) = split_cpv(store, &interner, &op.cpv) {
            let cp = match (interner.resolve(category), interner.resolve(package)) {
                (Some(c), Some(p)) => Some(format!("{c}/{p}")),
                _ => None,
            };
            store.remove(category, package, &version)?;
            if !op.replaced
                && let Some(cp) = cp
            {
                self.remove_from_world(&cp)?;
            }
        }

        recovery::clear_marker(&self.ctx.marker_dir())?;

        let reconciled = self.reconcile(store, registry)?;

        let report = PostMergeReport {
            elog: vec![format!("unmerged {}", op.cpv)],
            ..PostMergeReport::default()
        };

        Ok(OperationOutcome {
            cpv: op.cpv.clone(),
            merged: false,
            counter: None,
            report,
            preserved: Vec::new(),
            reconciled,
        })
    }

    /// Reconcile the registry against the current installed set, removing dropped
    /// libraries from disk and returning their paths.
    fn reconcile(
        &self,
        store: &Store,
        registry: &mut PreservedLibs,
    ) -> Result<Vec<String>, MergeError> {
        let span = tracing::info_span!("preserve_reconcile");
        let _enter = span.enter();
        let interner = store.interner();
        let dropped = preserve::reconcile(registry, store, interner);
        for path in &dropped {
            let live = self.ctx.live_path(path);
            match std::fs::remove_file(&live) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => return Err(MergeError::Io { path: live, source }),
            }
        }
        if !dropped.is_empty() {
            tracing::info!(count = dropped.len(), "dropped unused preserved libraries");
        }
        Ok(dropped)
    }

    /// Scan for an interrupted operation and recover it.
    ///
    /// An interrupted merge never made its record visible, so recovery clears the
    /// marker and leaves the store in its prior visible state; the just-placed
    /// files are harmless orphans removed when the operation is retried or
    /// overwritten. An interrupted unmerge is re-run idempotently by the caller's
    /// retry; recovery clears the marker so the next apply can proceed.
    pub fn recover(&self) -> Result<Option<recovery::Marker>, MergeError> {
        let Some(marker) = recovery::scan(&self.ctx.marker_dir())? else {
            return Ok(None);
        };
        let span = tracing::info_span!("recover", pkg = %marker.cpv, kind = ?marker.kind);
        let _enter = span.enter();
        tracing::warn!("recovering interrupted operation");

        match marker.kind {
            MarkerKind::Merge => {
                // The record never became visible; roll back to the prior visible
                // state by clearing the marker. Orphaned files, if any, are
                // overwritten on retry.
                tracing::info!("merge interrupted before commit; rolled back to prior state");
            }
            MarkerKind::Unmerge => {
                // Re-run the unmerge idempotently: it removes only paths still
                // owned and matching.
                let store = Store::load(self.store_paths())?;
                let interner = store.interner().clone();
                let _ = unmerge::unmerge(&self.ctx, &store, &interner, &marker.cpv)?;
                tracing::info!("unmerge re-run idempotently");
            }
        }
        recovery::clear_marker(&self.ctx.marker_dir())?;
        Ok(Some(marker))
    }

    /// Add `category/package` to the world file, keeping it sorted and unique.
    fn add_to_world(&self, category: &str, package: &str) -> Result<(), MergeError> {
        let cp = format!("{category}/{package}");
        let path = self.ctx.world_file();
        let mut set = read_world(&path)?;
        set.insert(cp);
        write_world(&path, &set)
    }

    /// Remove `category/package` from the world file.
    fn remove_from_world(&self, cp: &str) -> Result<(), MergeError> {
        let path = self.ctx.world_file();
        let mut set = read_world(&path)?;
        set.remove(cp);
        write_world(&path, &set)
    }
}

/// Read the world file into a sorted set, empty when absent.
fn read_world(path: &Path) -> Result<std::collections::BTreeSet<String>, MergeError> {
    if !path.exists() {
        return Ok(std::collections::BTreeSet::new());
    }
    let body = std::fs::read_to_string(path).with_path(path)?;
    Ok(body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Write the world set, one entry per line, atomically.
fn write_world(path: &Path, set: &std::collections::BTreeSet<String>) -> Result<(), MergeError> {
    let mut body = String::new();
    for cp in set {
        body.push_str(cp);
        body.push('\n');
    }
    moraine_common::fs::atomic_write(path, body.as_bytes())?;
    Ok(())
}

/// Split a `cpv` into the interned `(category, package, version)` of a matching
/// installed record, if one exists.
fn split_cpv(
    store: &Store,
    interner: &moraine_common::Interner,
    cpv: &str,
) -> Option<(moraine_common::Symbol, moraine_common::Symbol, String)> {
    store
        .records()
        .iter()
        .find(|r| r.cpv(interner) == cpv)
        .map(|r| (r.category, r.package, r.version.as_str().to_string()))
}
