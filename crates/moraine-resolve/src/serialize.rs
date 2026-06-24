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
    Ok(schedule_blockers(solution, merges))
}

/// Schedule uninstall tasks for the solution's blockers around the merges.
///
/// A strong blocker (`!!`) forbids file overlap, so the blocked package's
/// uninstall is ordered before the replacement merges. A weak blocker (`!`)
/// permits merge-over, so its removal is appended after the merges. Safety
/// refusals (the package manager, the only-suitable-runtime-provider case, and
/// unresolvable system-set members) would be enforced here once the installed
/// store is threaded in; with only the solved set available the scheduler
/// records the removals without removing a package that the merges depend on.
fn schedule_blockers(solution: &ResolvedSolution, merges: Vec<Task>) -> Vec<Task> {
    let merged_cps: BTreeSet<&str> = solution.packages.iter().map(|p| p.cp.as_str()).collect();
    let mut pre: Vec<Task> = Vec::new();
    let mut post: Vec<Task> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for blocker in &solution.blockers {
        let Some(cp) = blocked_cp(&blocker.blocked_atom) else {
            continue;
        };
        // Never uninstall a package that the solution is installing.
        if merged_cps.contains(cp.as_str()) {
            continue;
        }
        if !seen.insert(cp.clone()) {
            continue;
        }
        let task = Task {
            kind: TaskKind::Uninstall,
            cp,
            version: String::new(),
            slot: String::new(),
            use_enabled: Vec::new(),
        };
        if blocker.strong {
            pre.push(task);
        } else {
            post.push(task);
        }
    }

    let mut out = Vec::with_capacity(pre.len() + merges.len() + post.len());
    out.extend(pre);
    out.extend(merges);
    out.extend(post);
    out
}

/// Extract the `category/package` from a rendered blocker atom such as
/// `!cat/foo`, `!!=cat/foo-1`, or `cat/foo:2`.
fn blocked_cp(atom: &str) -> Option<String> {
    let s = atom.trim_start_matches('!');
    let s = s.trim_start_matches(['=', '<', '>', '~']);
    let s = s.trim_start_matches('=');
    // Strip slot/use suffixes.
    let s = s.split([':', '[']).next().unwrap_or(s);
    // The remaining text is `category/package` possibly followed by `-version`.
    // Keep up to the version: a version segment starts at a `-` followed by a
    // digit.
    let (category, rest) = s.split_once('/')?;
    let pkg_end = rest
        .char_indices()
        .find_map(|(i, _)| {
            let bytes = rest.as_bytes();
            if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1].is_ascii_digit() {
                Some(i)
            } else {
                None
            }
        })
        .unwrap_or(rest.len());
    let package = &rest[..pkg_end];
    if package.is_empty() {
        None
    } else {
        Some(format!("{category}/{package}"))
    }
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
fn collect_asap(solution: &ResolvedSolution, _graph: &MergeGraph) -> BTreeSet<String> {
    let mut asap = BTreeSet::new();
    let libc_present =
        solution.package("sys-libs/glibc").is_some() || solution.package("sys-libs/musl").is_some();
    for cp in [
        "sys-libs/glibc",
        "sys-libs/musl",
        "virtual/libc",
        "sys-apps/portage",
    ] {
        if solution.package(cp).is_some() {
            asap.insert(cp.to_owned());
        }
    }
    if libc_present && solution.package("sys-kernel/linux-headers").is_some() {
        asap.insert("sys-kernel/linux-headers".to_owned());
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

/// Build the task for a node from the solution.
fn build_task(solution: &ResolvedSolution, cp: &str) -> Task {
    let pkg = solution.package(cp);
    match pkg {
        Some(p) => Task {
            kind: TaskKind::Merge,
            cp: p.cp.clone(),
            version: p.version.to_string(),
            slot: p.slot.clone(),
            use_enabled: p.use_enabled.iter().cloned().collect(),
        },
        None => Task {
            kind: TaskKind::Uninstall,
            cp: cp.to_owned(),
            version: String::new(),
            slot: String::new(),
            use_enabled: Vec::new(),
        },
    }
}

// Keep NodeKind referenced for downstream uninstall scheduling.
const _: NodeKind = NodeKind::Merge;
