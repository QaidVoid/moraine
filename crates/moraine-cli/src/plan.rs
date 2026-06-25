//! Building a presentation [`MergePlan`] from the resolved plan.
//!
//! `moraine-resolve` returns a minimal ordered task list. This module joins each
//! task back to its [`ResolvedPackage`] for slot and USE detail, to the
//! dependency edges for tree parents, and to the [`ResolveSource`] for installed
//! state, then derives the operation kind, version change, and USE diff the
//! renderer needs. It computes no resolution decisions of its own. Being generic
//! over [`ResolveSource`] keeps it testable with a fake source.

use std::collections::BTreeSet;

use moraine_resolve::{Acceptability, ResolveSource, ResolvedSolution, Task, TaskKind};
use tracing::instrument;

use crate::render::{Acceptance, MergeEntry, MergePlan, Operation, UseFlag};

/// Build a [`MergePlan`] from the ordered tasks, solution, and source.
///
/// `tasks` is the serialized merge order. The solution supplies per-package slot
/// and USE detail and the dependency edges used for tree parents. The source
/// supplies installed versions and USE, used to classify operations and compute
/// the USE diff.
#[instrument(skip(tasks, solution, source), fields(tasks = tasks.len()))]
pub fn build_plan<S: ResolveSource>(
    tasks: &[Task],
    solution: &ResolvedSolution,
    source: &S,
) -> MergePlan {
    let entries = tasks
        .iter()
        .map(|task| build_entry(task, solution, source))
        .collect();
    MergePlan { entries }
}

/// Build a single merge entry for one task.
fn build_entry<S: ResolveSource>(
    task: &Task,
    solution: &ResolvedSolution,
    source: &S,
) -> MergeEntry {
    if task.kind == TaskKind::Uninstall {
        return MergeEntry {
            cp: task.cp.clone(),
            version: task.version.clone(),
            old_version: None,
            operation: Operation::Uninstall,
            acceptance: Acceptance::Stable,
            slot: task.slot.clone(),
            subslot: None,
            repository: None,
            binary: false,
            fetched: false,
            build_id: None,
            use_flags: Vec::new(),
            fetch_size: None,
            parents: parents_of(&task.cp, solution),
        };
    }

    let resolved = solution.package(&task.cp);
    let installed = source.installed(&task.cp);
    let installed_same_slot = installed.iter().find(|i| i.slot == task.slot);

    let old_version = installed_same_slot
        .map(|i| i.version.as_str().to_owned())
        .filter(|old| old != &task.version);
    let operation = classify(task, resolved, installed_same_slot);

    let enabled: BTreeSet<String> = task.use_enabled.iter().cloned().collect();
    let installed_use: BTreeSet<String> = installed_same_slot
        .map(|i| i.use_enabled.clone())
        .unwrap_or_default();
    // The selected candidate's metadata drives the display universe (its full
    // IUSE so disabled flags show as `-flag`) and the keyword acceptance marker.
    let meta = source
        .versions_of(&task.cp)
        .into_iter()
        .find(|m| m.version.as_str() == task.version && m.slot == task.slot);
    let iuse: BTreeSet<String> = meta
        .as_ref()
        .map(|m| m.iuse.clone())
        .unwrap_or_else(|| enabled.clone());
    // A package the resolver could only reach via a `~arch` keyword is marked
    // testing (`~`), matching `emerge`; a license-only change keeps it stable.
    let acceptance = match meta.as_ref().map(|m| source.acceptability(m)) {
        Some(Acceptability::NeedsAccept(c)) if c.keyword.as_deref() == Some("**") => {
            Acceptance::Masked
        }
        Some(Acceptability::NeedsAccept(c)) if c.keyword.is_some() => Acceptance::Testing,
        _ => Acceptance::Stable,
    };
    let use_flags = use_diff(
        &enabled,
        &installed_use,
        &iuse,
        installed_same_slot.is_some(),
    );

    MergeEntry {
        cp: task.cp.clone(),
        version: task.version.clone(),
        old_version,
        operation,
        acceptance,
        slot: task.slot.clone(),
        subslot: resolved.and_then(|r| r.subslot.clone()),
        repository: None,
        binary: false,
        fetched: false,
        build_id: None,
        use_flags,
        fetch_size: None,
        parents: parents_of(&task.cp, solution),
    }
}

/// Classify the operation kind from installed state.
fn classify(
    task: &Task,
    resolved: Option<&moraine_resolve::ResolvedPackage>,
    installed: Option<&moraine_resolve::InstalledMeta>,
) -> Operation {
    if let Some(pkg) = resolved
        && pkg.subslot_rebuild
    {
        return Operation::Rebuild;
    }
    let Some(installed) = installed else {
        return Operation::New;
    };
    let installed_version = installed.version.as_str();
    if installed_version == task.version {
        return Operation::Reinstall;
    }
    match moraine_version::Version::parse(&task.version) {
        Ok(new) if new > installed.version => Operation::Upgrade,
        Ok(_) => Operation::Downgrade,
        Err(_) => Operation::Reinstall,
    }
}

/// Compute the USE-flag diff against installed state.
fn use_diff(
    enabled: &BTreeSet<String>,
    installed: &BTreeSet<String>,
    universe: &BTreeSet<String>,
    has_installed: bool,
) -> Vec<UseFlag> {
    let mut all: BTreeSet<String> = enabled.iter().cloned().collect();
    all.extend(universe.iter().cloned());
    all.extend(installed.iter().cloned());

    let mut flags = Vec::new();
    for name in all {
        let is_enabled = enabled.contains(&name);
        let was_enabled = installed.contains(&name);
        let in_build = enabled.contains(&name) || universe.contains(&name);
        if !in_build && was_enabled {
            flags.push(UseFlag {
                name,
                enabled: false,
                changed: false,
                removed: true,
                group: None,
                hidden: false,
                forced: false,
            });
            continue;
        }
        let changed = has_installed && is_enabled != was_enabled;
        flags.push(UseFlag {
            name,
            enabled: is_enabled,
            changed,
            removed: false,
            group: None,
            hidden: false,
            forced: false,
        });
    }
    flags
}

/// The `category/package` values that pulled `cp` in, from the edges.
fn parents_of(cp: &str, solution: &ResolvedSolution) -> Vec<String> {
    let mut parents: Vec<String> = solution
        .edges
        .iter()
        .filter(|edge| edge.to == cp)
        .map(|edge| edge.from.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    parents.sort();
    parents
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use moraine_resolve::{
        DepClass, DepEdge, InstalledMeta, PackageMeta, ResolveSource, ResolvedPackage,
        ResolvedSolution, Root, Task, TaskKind,
    };
    use moraine_version::Version;

    use super::*;
    use crate::render::Operation;

    /// A fake source that only carries installed state for plan building.
    #[derive(Default)]
    struct FakeSource {
        installed: Vec<InstalledMeta>,
    }

    impl ResolveSource for FakeSource {
        fn versions_of(&self, _cp: &str) -> Vec<PackageMeta> {
            Vec::new()
        }
        fn is_visible(&self, _meta: &PackageMeta) -> bool {
            true
        }
        fn resolved_use(&self, _meta: &PackageMeta) -> BTreeSet<String> {
            BTreeSet::new()
        }
        fn is_provided(&self, _cp: &str, _version: &Version) -> bool {
            false
        }
        fn installed(&self, cp: &str) -> Vec<InstalledMeta> {
            self.installed
                .iter()
                .filter(|i| i.cp == cp)
                .cloned()
                .collect()
        }
    }

    fn installed(cp: &str, version: &str, use_enabled: &[&str]) -> InstalledMeta {
        InstalledMeta {
            cp: cp.to_owned(),
            version: Version::parse(version).unwrap(),
            slot: "0".to_owned(),
            subslot: None,
            use_enabled: use_enabled.iter().map(|s| s.to_string()).collect(),
            iuse: use_enabled.iter().map(|s| s.to_string()).collect(),
            slot_bindings: Vec::new(),
        }
    }

    fn resolved(cp: &str, version: &str, use_enabled: &[&str]) -> ResolvedPackage {
        ResolvedPackage {
            cp: cp.to_owned(),
            version: Version::parse(version).unwrap(),
            slot: "0".to_owned(),
            subslot: None,
            use_enabled: use_enabled.iter().map(|s| s.to_string()).collect(),
            slot_bindings: Vec::new(),
            already_installed: false,
            subslot_rebuild: false,
        }
    }

    fn merge_task(cp: &str, version: &str, use_enabled: &[&str]) -> Task {
        Task {
            kind: TaskKind::Merge,
            cp: cp.to_owned(),
            version: version.to_owned(),
            slot: "0".to_owned(),
            use_enabled: use_enabled.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn new_install_when_not_installed() {
        let sol = ResolvedSolution {
            packages: vec![resolved("cat/a", "1.0", &[])],
            ..Default::default()
        };
        let plan = build_plan(
            &[merge_task("cat/a", "1.0", &[])],
            &sol,
            &FakeSource::default(),
        );
        assert_eq!(plan.entries[0].operation, Operation::New);
        assert!(plan.entries[0].old_version.is_none());
    }

    #[test]
    fn upgrade_shows_old_version() {
        let sol = ResolvedSolution {
            packages: vec![resolved("cat/a", "2.0", &[])],
            ..Default::default()
        };
        let source = FakeSource {
            installed: vec![installed("cat/a", "1.0", &[])],
        };
        let plan = build_plan(&[merge_task("cat/a", "2.0", &[])], &sol, &source);
        assert_eq!(plan.entries[0].operation, Operation::Upgrade);
        assert_eq!(plan.entries[0].old_version.as_deref(), Some("1.0"));
    }

    #[test]
    fn downgrade_classified() {
        let sol = ResolvedSolution {
            packages: vec![resolved("cat/a", "1.0", &[])],
            ..Default::default()
        };
        let source = FakeSource {
            installed: vec![installed("cat/a", "2.0", &[])],
        };
        let plan = build_plan(&[merge_task("cat/a", "1.0", &[])], &sol, &source);
        assert_eq!(plan.entries[0].operation, Operation::Downgrade);
    }

    #[test]
    fn reinstall_same_version() {
        let sol = ResolvedSolution {
            packages: vec![resolved("cat/a", "1.0", &[])],
            ..Default::default()
        };
        let source = FakeSource {
            installed: vec![installed("cat/a", "1.0", &[])],
        };
        let plan = build_plan(&[merge_task("cat/a", "1.0", &[])], &sol, &source);
        assert_eq!(plan.entries[0].operation, Operation::Reinstall);
    }

    #[test]
    fn use_diff_marks_changes() {
        let sol = ResolvedSolution {
            packages: vec![resolved("cat/a", "1.0", &["ssl", "zlib"])],
            ..Default::default()
        };
        let source = FakeSource {
            installed: vec![installed("cat/a", "1.0", &["zlib"])],
        };
        let plan = build_plan(
            &[merge_task("cat/a", "1.0", &["ssl", "zlib"])],
            &sol,
            &source,
        );
        let ssl = plan.entries[0]
            .use_flags
            .iter()
            .find(|f| f.name == "ssl")
            .unwrap();
        assert!(ssl.enabled && ssl.changed);
        let zlib = plan.entries[0]
            .use_flags
            .iter()
            .find(|f| f.name == "zlib")
            .unwrap();
        assert!(zlib.enabled && !zlib.changed);
    }

    #[test]
    fn parents_come_from_edges() {
        let sol = ResolvedSolution {
            packages: vec![resolved("lib/dep", "1.0", &[])],
            edges: vec![DepEdge {
                from: "app/top".to_owned(),
                to: "lib/dep".to_owned(),
                class: DepClass::Rdepend,
                root: Root::Target,
                build_time: false,
                slot_op: false,
                optional: false,
            }],
            ..Default::default()
        };
        let plan = build_plan(
            &[merge_task("lib/dep", "1.0", &[])],
            &sol,
            &FakeSource::default(),
        );
        assert_eq!(plan.entries[0].parents, vec!["app/top".to_owned()]);
    }

    #[test]
    fn uninstall_task_becomes_uninstall_entry() {
        let task = Task {
            kind: TaskKind::Uninstall,
            cp: "cat/old".to_owned(),
            version: String::new(),
            slot: String::new(),
            use_enabled: Vec::new(),
        };
        let plan = build_plan(
            &[task],
            &ResolvedSolution::default(),
            &FakeSource::default(),
        );
        assert_eq!(plan.entries[0].operation, Operation::Uninstall);
    }
}
