//! Merge-order serialization: turning the priority-aware graph into a
//! deterministic ordered list of merge and uninstall tasks.
//!
//! The serializer runs an iterative leaf-extraction loop over a working copy of
//! the graph. Each iteration escalates the ignore tier from strictest to
//! medium-soft until a leaf appears, then resets. Three loosening stages run in
//! succession: prefer-ASAP, normal, and drop-satisfied. When no leaf exists, the
//! smallest runtime cycle is gathered and emitted weakest-edge-first. A residual
//! unbreakable cycle is reported as a structured diagnostic.

use std::collections::BTreeSet;

use tracing::instrument;

use crate::error::{MergeOrderError, ResidualCycle};
use crate::graph::{MAX_TIER, MergeGraph, NodeKind, Range};
use crate::solution::ResolvedSolution;

/// The kind of a serialized task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// Merge (install) the package.
    Merge,
    /// Uninstall the package.
    Uninstall,
}

/// A single ordered task in the merge plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// The task kind.
    pub kind: TaskKind,
    /// The `category/package`.
    pub cp: String,
    /// The chosen version, as a string.
    pub version: String,
    /// The chosen slot.
    pub slot: String,
    /// The enabled USE flags.
    pub use_enabled: Vec<String>,
}

/// The loosening stage of the serializer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    PreferAsap,
    Normal,
    DropSatisfied,
}

/// Serialize the resolved solution into an ordered task list.
#[instrument(skip(solution), fields(packages = solution.packages.len()))]
pub fn serialize(solution: &ResolvedSolution) -> Result<Vec<Task>, MergeOrderError> {
    let mut graph = MergeGraph::from_solution(solution);
    let asap = collect_asap(solution, &graph);

    let mut order: Vec<String> = Vec::new();
    let mut stage = Stage::PreferAsap;
    let mut asap_pending: BTreeSet<String> = asap.clone();

    while !graph.is_empty() {
        // Refresh pending ASAP set against remaining nodes.
        asap_pending.retain(|cp| graph.node(cp).is_some());

        if let Some(selected) = select_one(&graph, stage, &asap_pending) {
            order.push(selected.clone());
            graph.remove(&selected);
            asap_pending.remove(&selected);
            // Reset to the strictest stage after a successful selection.
            stage = Stage::PreferAsap;
            continue;
        }

        // No plain leaf: try to break the smallest cycle.
        if let Some(cycle) = smallest_cycle(&graph, stage) {
            emit_cycle(&mut graph, &cycle, solution, &mut order);
            stage = Stage::PreferAsap;
            continue;
        }

        // Advance the loosening stage.
        match stage {
            Stage::PreferAsap => stage = Stage::Normal,
            Stage::Normal => stage = Stage::DropSatisfied,
            Stage::DropSatisfied => {
                // Every stage exhausted with a non-empty graph: residual cycle.
                return Err(MergeOrderError::UnresolvableCycle(residual(&graph)));
            }
        }
    }

    let merges: Vec<Task> = order.iter().map(|cp| build_task(solution, cp)).collect();
    schedule_blockers(solution, merges)
}

/// The package manager's own `category/package`, which a blocker uninstall may
/// never remove.
const PACKAGE_MANAGER: &str = "sys-apps/portage";

/// Schedule uninstall tasks for the solution's blockers around the merges.
///
/// Each blocker carries the exact installed entries its atom matched (filtered
/// by version and slot when the victims were computed), so an uninstall removes
/// only those entries, never the whole cp. A strong blocker (`!!`) forbids file
/// overlap, so its victims are removed before the replacement merges; a weak
/// blocker (`!`) permits merge-over, so its victims are removed after.
///
/// Before scheduling a removal, two safety rules apply: the package manager
/// (`sys-apps/portage`) is never removed, and a victim that is the sole provider
/// of a `cp` a surviving package still depends on is refused. Either refusal
/// returns a structured [`MergeOrderError::UnsafeOperation`] rather than emitting
/// a destructive uninstall.
fn schedule_blockers(
    solution: &ResolvedSolution,
    merges: Vec<Task>,
) -> Result<Vec<Task>, MergeOrderError> {
    let mut pre: Vec<Task> = Vec::new();
    let mut post: Vec<Task> = Vec::new();
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();

    for blocker in &solution.blockers {
        // Uninstall only the specific installed entries the blocker's atom
        // matches, by version and slot, never the whole cp.
        for victim in &blocker.victims {
            // Safety: never remove the package manager itself.
            if victim.cp == PACKAGE_MANAGER {
                return Err(MergeOrderError::UnsafeOperation(format!(
                    "blocker {} would uninstall the package manager {}",
                    blocker.blocker, victim.cp
                )));
            }
            // Safety: refuse to remove the sole provider of a cp a surviving
            // package in the solution still depends on. A surviving instance of
            // the same cp (another slot kept in the solution) makes the removal
            // safe; otherwise a dependent would be left unsatisfied.
            let has_surviving_provider = solution.packages.iter().any(|p| p.cp == victim.cp);
            let has_dependent = solution
                .edges
                .iter()
                .any(|e| crate::solution::endpoint_cp(&e.to) == victim.cp);
            if has_dependent && !has_surviving_provider {
                return Err(MergeOrderError::UnsafeOperation(format!(
                    "blocker {} would uninstall {}, the sole provider a surviving package depends on",
                    blocker.blocker, victim.cp
                )));
            }

            let key = (
                victim.cp.clone(),
                victim.version.as_str().to_owned(),
                victim.slot.clone(),
            );
            if !seen.insert(key) {
                continue;
            }
            let task = Task {
                kind: TaskKind::Uninstall,
                cp: victim.cp.clone(),
                version: victim.version.as_str().to_owned(),
                slot: victim.slot.clone(),
                use_enabled: Vec::new(),
            };
            // A strong blocker forbids file overlap, so its victim is removed
            // before the merges; a weak blocker permits merge-over and is removed
            // after.
            if blocker.strong {
                pre.push(task);
            } else {
                post.push(task);
            }
        }
    }

    let mut out = Vec::with_capacity(pre.len() + merges.len() + post.len());
    out.extend(pre);
    out.extend(merges);
    out.extend(post);
    Ok(out)
}

/// The range a stage searches under.
fn stage_range(stage: Stage, asap_pending: &BTreeSet<String>) -> Range {
    match stage {
        // While ASAP nodes are pending, search the satisfied range so soft
        // satisfied edges do not block an ASAP node; otherwise normal.
        Stage::PreferAsap => {
            if asap_pending.is_empty() {
                Range::Normal
            } else {
                Range::Satisfied
            }
        }
        Stage::Normal => Range::Normal,
        Stage::DropSatisfied => Range::Satisfied,
    }
}

/// Select a single node for emission under the given stage, trying increasing
/// ignore tiers and preferring ASAP nodes and installed leaves deterministically.
fn select_one(graph: &MergeGraph, stage: Stage, asap_pending: &BTreeSet<String>) -> Option<String> {
    let range = stage_range(stage, asap_pending);
    for tier in 0..=MAX_TIER {
        let leaves = graph.leaves(tier, range);
        if leaves.is_empty() {
            continue;
        }
        // Prefer an ASAP leaf when in the prefer-asap stage.
        if stage == Stage::PreferAsap
            && let Some(pick) = leaves.iter().find(|c| asap_pending.contains(*c))
        {
            return Some(pick.clone());
        }
        // Deterministic tie-break: the first leaf in sorted order. `leaves` is
        // already sorted because the graph stores nodes in a BTreeMap.
        return leaves.into_iter().next();
    }
    None
}

/// Find the smallest runtime cycle group that can be made into a leaf-set by
/// ignoring soft edges, preferring smaller groups.
fn smallest_cycle(graph: &MergeGraph, stage: Stage) -> Option<Vec<String>> {
    let range = stage_range(stage, &BTreeSet::new());
    // Only meaningful when no plain leaf exists. Gather runtime clusters and
    // pick the smallest non-trivial one whose members can all be emitted once
    // intra-cluster soft edges are ignored at the top tier.
    let mut best: Option<Vec<String>> = None;
    for start in graph.node_keys() {
        let cluster = graph.gather_runtime(&start);
        if cluster.len() < 2 {
            continue;
        }
        // Confirm the cluster is a real cycle: every member must be reachable
        // back to the start via runtime edges (gather already approximates
        // this). Require that ignoring soft edges at MAX_TIER yields at least
        // one leaf within the cluster.
        let has_breakable = cluster.iter().any(|cp| {
            graph
                .edges_of(cp)
                .iter()
                .all(|(_, f)| crate::graph::edge_ignored(f, MAX_TIER, range))
        });
        if !has_breakable {
            continue;
        }
        let v: Vec<String> = cluster.into_iter().collect();
        match &best {
            Some(b) if b.len() <= v.len() => {}
            _ => best = Some(v),
        }
    }
    best
}

/// Emit the members of a cycle group in weakest-edge-first leaf order,
/// preferring installed leaves.
fn emit_cycle(
    graph: &mut MergeGraph,
    cycle: &[String],
    solution: &ResolvedSolution,
    order: &mut Vec<String>,
) {
    let mut members: BTreeSet<String> = cycle.iter().cloned().collect();
    while !members.is_empty() {
        // Find a member whose only outgoing edges within the cluster are soft
        // (ignorable at MAX_TIER under the satisfied range).
        let mut pick: Option<String> = None;
        // Prefer an installed leaf.
        for cp in &members {
            let already = solution
                .package(cp)
                .map(|p| p.already_installed)
                .unwrap_or(false);
            let breakable = graph
                .edges_of(cp)
                .iter()
                .filter(|(to, _)| members.contains(to))
                .all(|(_, f)| crate::graph::edge_ignored(f, MAX_TIER, Range::Satisfied));
            if breakable {
                if already {
                    pick = Some(cp.clone());
                    break;
                } else if pick.is_none() {
                    pick = Some(cp.clone());
                }
            }
        }
        let chosen = pick.unwrap_or_else(|| members.iter().next().cloned().expect("non-empty"));
        order.push(chosen.clone());
        graph.remove(&chosen);
        members.remove(&chosen);
    }
}

/// Collect the ASAP set: libc (expanded through virtuals from the graph), OS
/// headers when a libc upgrade is present, and the package manager replacement.
/// The set holds slot-qualified node keys, since the graph is keyed by `cp:slot`.
fn collect_asap(solution: &ResolvedSolution, _graph: &MergeGraph) -> BTreeSet<String> {
    let mut asap = BTreeSet::new();
    // Insert every selected slot of `cp` as a slot-qualified node key.
    let mut insert_cp = |cp: &str| {
        for p in solution.packages.iter().filter(|p| p.cp == cp) {
            asap.insert(crate::provider::package_key(&p.cp, &p.slot));
        }
    };
    let libc_present =
        solution.package("sys-libs/glibc").is_some() || solution.package("sys-libs/musl").is_some();
    for cp in [
        "sys-libs/glibc",
        "sys-libs/musl",
        "virtual/libc",
        "sys-apps/portage",
    ] {
        insert_cp(cp);
    }
    if libc_present && solution.package("sys-kernel/linux-headers").is_some() {
        insert_cp("sys-kernel/linux-headers");
    }
    asap
}

/// Build a structured residual-cycle diagnostic from the remaining graph.
fn residual(graph: &MergeGraph) -> ResidualCycle {
    let packages = graph.node_keys();
    let mut edges = Vec::new();
    for from in &packages {
        for (to, flags) in graph.edges_of(from) {
            let class = if flags.buildtime || flags.buildtime_slot_op {
                "buildtime"
            } else if flags.runtime || flags.runtime_slot_op {
                "runtime"
            } else if flags.runtime_post {
                "runtime_post"
            } else if flags.installtime {
                "installtime"
            } else {
                "optional"
            };
            edges.push((from.clone(), to.clone(), class.to_owned()));
        }
    }
    ResidualCycle { packages, edges }
}

/// Build the task for a node from the solution, looking the package up by its
/// slot-qualified `cp:slot` key so the matching slot's version and USE are used.
fn build_task(solution: &ResolvedSolution, key: &str) -> Task {
    match solution.package_by_key(key) {
        Some(p) => Task {
            kind: TaskKind::Merge,
            cp: p.cp.clone(),
            version: p.version.to_string(),
            slot: p.slot.clone(),
            use_enabled: p.use_enabled.iter().cloned().collect(),
        },
        None => Task {
            kind: TaskKind::Uninstall,
            cp: crate::solution::endpoint_cp(key).to_owned(),
            version: String::new(),
            slot: String::new(),
            use_enabled: Vec::new(),
        },
    }
}

// Keep NodeKind referenced for downstream uninstall scheduling.
const _: NodeKind = NodeKind::Merge;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solution::{BlockVictim, DepClass, DepEdge, RecordedBlocker, ResolvedPackage, Root};
    use moraine_version::Version;

    fn pkg(cp: &str, version: &str) -> ResolvedPackage {
        ResolvedPackage {
            cp: cp.to_owned(),
            version: Version::parse(version).unwrap(),
            slot: "0".to_owned(),
            subslot: None,
            use_enabled: Default::default(),
            slot_bindings: Vec::new(),
            already_installed: false,
            subslot_rebuild: false,
        }
    }

    #[test]
    fn refuses_removing_sole_provider_of_a_surviving_dependent() {
        // cat/m survives and depends on cat/x, but a blocker would remove the
        // only cat/x. The removal is refused as unsafe.
        let solution = ResolvedSolution {
            packages: vec![pkg("cat/m", "1")],
            edges: vec![DepEdge {
                from: "cat/m".to_owned(),
                to: "cat/x".to_owned(),
                class: DepClass::Rdepend,
                root: Root::Target,
                build_time: false,
                slot_op: false,
                optional: false,
            }],
            blockers: vec![RecordedBlocker {
                blocker: "cat/other".to_owned(),
                blocked_atom: "cat/x".to_owned(),
                strong: false,
                victims: vec![BlockVictim {
                    cp: "cat/x".to_owned(),
                    version: Version::parse("1").unwrap(),
                    slot: "0".to_owned(),
                }],
            }],
            backtracks: 0,
            autounmask: Vec::new(),
        };

        let err = serialize(&solution).expect_err("the unsafe removal is refused");
        assert!(matches!(err, MergeOrderError::UnsafeOperation(_)));
    }

    #[test]
    fn allows_removal_when_no_surviving_dependent() {
        // No edge points at cat/x, so removing it is safe.
        let solution = ResolvedSolution {
            packages: vec![pkg("cat/m", "1")],
            edges: Vec::new(),
            blockers: vec![RecordedBlocker {
                blocker: "cat/m".to_owned(),
                blocked_atom: "cat/x".to_owned(),
                strong: true,
                victims: vec![BlockVictim {
                    cp: "cat/x".to_owned(),
                    version: Version::parse("1").unwrap(),
                    slot: "0".to_owned(),
                }],
            }],
            backtracks: 0,
            autounmask: Vec::new(),
        };

        let tasks = serialize(&solution).expect("safe removal scheduled");
        assert!(
            tasks
                .iter()
                .any(|t| t.kind == TaskKind::Uninstall && t.cp == "cat/x"),
            "cat/x is scheduled for uninstall: {tasks:?}"
        );
    }
}
