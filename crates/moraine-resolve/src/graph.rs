//! The priority-aware dependency graph for merge ordering.
//!
//! Each edge carries a flag-set mirroring Portage's `DepPriority` slots. Hardness
//! is a pure function of the flags. Two ignore ladders, the normal range and the
//! satisfied range, are expressed as tiers; the leaf query returns nodes whose
//! every outgoing edge is at or below a caller-supplied ignore tier.

use std::collections::{BTreeMap, BTreeSet};

use crate::solution::{DepClass, ResolvedSolution};

/// The kind of a graph node: a package merge or an uninstall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NodeKind {
    /// A package to merge.
    Merge,
    /// A package to uninstall.
    Uninstall,
}

/// A node in the merge graph: a package operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeNode {
    /// The `category/package`.
    pub cp: String,
    /// The operation kind.
    pub kind: NodeKind,
    /// The chosen slot.
    pub slot: String,
}

/// The flag-set carried by a dependency edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EdgeFlags {
    /// Build-time dependency (`DEPEND`/`BDEPEND`).
    pub buildtime: bool,
    /// Build-time slot-operator dependency.
    pub buildtime_slot_op: bool,
    /// Runtime dependency (`RDEPEND`).
    pub runtime: bool,
    /// Runtime slot-operator dependency.
    pub runtime_slot_op: bool,
    /// Post-merge runtime dependency (`PDEPEND`).
    pub runtime_post: bool,
    /// Install-time dependency (`IDEPEND`), ranked within the runtime tier.
    pub installtime: bool,
    /// Optional `||`-branch-derived dependency.
    pub optional: bool,
    /// The target is already provided by an installed package.
    pub satisfied: bool,
    /// Cross-root / cross-prefix dependency.
    pub cross: bool,
    /// The edge has been ignored during serialization.
    pub ignored: bool,
}

impl EdgeFlags {
    /// Build the flags for a dependency class, slot-operator marker, and
    /// optional marker.
    pub fn for_class(class: DepClass, slot_op: bool, optional: bool, satisfied: bool) -> Self {
        let mut f = EdgeFlags {
            optional,
            satisfied,
            ..Default::default()
        };
        match class {
            DepClass::Bdepend | DepClass::Depend => {
                f.buildtime = true;
                f.buildtime_slot_op = slot_op;
            }
            DepClass::Rdepend => {
                f.runtime = true;
                f.runtime_slot_op = slot_op;
            }
            DepClass::Idepend => {
                f.installtime = true;
                f.runtime_slot_op = slot_op;
            }
            DepClass::Pdepend => {
                f.runtime_post = true;
            }
        }
        f
    }

    /// The hardness of this edge as a pure function of its flags, matching the
    /// reference ranking: `buildtime_slot_op` hardest, then `buildtime`, then
    /// `runtime_slot_op`, then `runtime`/`installtime`, then `runtime_post`,
    /// then `optional`, then unflagged.
    pub fn hardness(&self) -> i32 {
        if self.buildtime_slot_op {
            7
        } else if self.buildtime {
            6
        } else if self.runtime_slot_op {
            5
        } else if self.runtime || self.installtime {
            4
        } else if self.runtime_post {
            3
        } else if self.optional {
            2
        } else {
            1
        }
    }
}

/// The ignore range a tier belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// The normal ignore ladder, which does not consider satisfied edges.
    Normal,
    /// The satisfied ignore ladder, which additionally drops satisfied edges.
    Satisfied,
}

/// The maximum ignore tier. Tier 0 ignores nothing; higher tiers ignore softer
/// edges up to medium-soft (`runtime_post`).
pub const MAX_TIER: u32 = 5;

/// Whether an edge is ignored at the given tier under the given range.
///
/// The normal range ignores, in increasing tiers: nothing, then `optional`,
/// then `runtime_post`, then `installtime`, then `runtime` (non-slot-op), then
/// `runtime_slot_op`. The satisfied range additionally drops any `satisfied`
/// edge once it would otherwise be selectable.
pub fn edge_ignored(flags: &EdgeFlags, tier: u32, range: Range) -> bool {
    if flags.ignored {
        return true;
    }
    // The satisfied range drops satisfied edges at every tier.
    if range == Range::Satisfied && flags.satisfied {
        return true;
    }
    // A build-time edge is never ignored in either range's normal tiers; a
    // hard-only cycle must escalate to the satisfied stage.
    if flags.buildtime || flags.buildtime_slot_op {
        return false;
    }
    let h = flags.hardness();
    // At tier t, ignore every edge whose hardness is at or below the tier's
    // threshold. Map tiers to hardness thresholds for runtime-and-softer edges.
    let threshold = match tier {
        0 => 0, // ignore nothing
        1 => 2, // optional
        2 => 3, // + runtime_post
        3 => 4, // + runtime / installtime
        4 => 5, // + runtime_slot_op
        _ => 5, // medium-soft cap
    };
    h <= threshold && h < 6
}

/// The merge graph: nodes plus class-tagged edges, with indexed adjacency.
#[derive(Debug, Clone)]
pub struct MergeGraph {
    /// The nodes, keyed by `category/package`.
    nodes: BTreeMap<String, MergeNode>,
    /// Outgoing edges per node: `from -> [(to, flags)]`.
    out: BTreeMap<String, Vec<(String, EdgeFlags)>>,
    /// Incoming-edge count cache per node (number of distinct dependents).
    incoming: BTreeMap<String, BTreeSet<String>>,
}

impl MergeGraph {
    /// All node keys in deterministic order.
    pub fn node_keys(&self) -> Vec<String> {
        self.nodes.keys().cloned().collect()
    }

    /// The node for a key.
    pub fn node(&self, cp: &str) -> Option<&MergeNode> {
        self.nodes.get(cp)
    }

    /// The number of nodes remaining.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the graph is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The outgoing edges of a node.
    pub fn edges_of(&self, cp: &str) -> &[(String, EdgeFlags)] {
        self.out.get(cp).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Build the graph from a resolved solution. An edge whose target is not a
    /// merged node is dropped (already satisfied by the environment), and an
    /// edge whose target package is already installed at the same identity is
    /// marked `satisfied`.
    pub fn from_solution(solution: &ResolvedSolution) -> Self {
        // Nodes are keyed by the slot-qualified `cp:slot` key, so two slots of one
        // cp are distinct nodes that both reach the merge plan.
        let mut nodes: BTreeMap<String, MergeNode> = BTreeMap::new();
        let mut by_cp: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for p in &solution.packages {
            let key = crate::provider::package_key(&p.cp, &p.slot);
            by_cp.entry(p.cp.as_str()).or_default().push(key.clone());
            nodes.insert(
                key,
                MergeNode {
                    cp: p.cp.clone(),
                    kind: NodeKind::Merge,
                    slot: p.slot.clone(),
                },
            );
        }

        let mut out: BTreeMap<String, Vec<(String, EdgeFlags)>> = BTreeMap::new();
        let mut incoming: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for key in nodes.keys() {
            out.entry(key.clone()).or_default();
            incoming.entry(key.clone()).or_default();
        }

        // Resolve an edge endpoint to concrete node keys. A slot-qualified endpoint
        // that is itself a node key resolves to that node; a bare `cp` endpoint
        // (from a hand-built solution) expands to every slot-node of that cp.
        let resolve_endpoint = |endpoint: &str| -> Vec<String> {
            if nodes.contains_key(endpoint) {
                vec![endpoint.to_owned()]
            } else {
                by_cp.get(endpoint).cloned().unwrap_or_default()
            }
        };

        for edge in &solution.edges {
            for from in resolve_endpoint(&edge.from) {
                for to in resolve_endpoint(&edge.to) {
                    // A package flagged for a slot-operator rebuild is reinstalled
                    // even though its version is unchanged, so an edge into it is
                    // real, not a no-op satisfied edge.
                    let satisfied = solution
                        .package_by_key(&to)
                        .map(|p| p.already_installed && !p.subslot_rebuild)
                        .unwrap_or(false);
                    let flags =
                        EdgeFlags::for_class(edge.class, edge.slot_op, edge.optional, satisfied);
                    out.entry(from.clone())
                        .or_default()
                        .push((to.clone(), flags));
                    incoming.entry(to).or_default().insert(from.clone());
                }
            }
        }

        MergeGraph {
            nodes,
            out,
            incoming,
        }
    }

    /// Whether a node has any dependents (incoming edges) among remaining nodes.
    pub fn has_dependents(&self, cp: &str) -> bool {
        self.incoming
            .get(cp)
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }

    /// The leaf nodes at a given ignore tier and range: nodes with no outgoing
    /// edge that survives the tier (i.e. every outgoing edge is ignored).
    pub fn leaves(&self, tier: u32, range: Range) -> Vec<String> {
        let mut leaves = Vec::new();
        for cp in self.nodes.keys() {
            let has_live = self
                .edges_of(cp)
                .iter()
                .any(|(_, flags)| !edge_ignored(flags, tier, range));
            if !has_live {
                leaves.push(cp.clone());
            }
        }
        leaves
    }

    /// Remove a node and all edges touching it, returning the removed node.
    pub fn remove(&mut self, cp: &str) -> Option<MergeNode> {
        let node = self.nodes.remove(cp)?;
        self.out.remove(cp);
        self.incoming.remove(cp);
        for (_, targets) in self.out.iter_mut() {
            targets.retain(|(to, _)| to != cp);
        }
        for (_, deps) in self.incoming.iter_mut() {
            deps.remove(cp);
        }
        Some(node)
    }

    /// The nodes a node runtime-depends on (ignoring soft edges below the given
    /// tier), used by runtime grouping and cycle gathering.
    pub fn runtime_children(&self, cp: &str) -> Vec<String> {
        self.edges_of(cp)
            .iter()
            .filter(|(_, f)| (f.runtime || f.runtime_slot_op || f.installtime) && !f.ignored)
            .map(|(to, _)| to.clone())
            .collect()
    }

    /// Recursively gather a node together with the nodes it runtime-depends on
    /// into one set (a runtime cluster / candidate cycle group).
    pub fn gather_runtime(&self, start: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![start.to_owned()];
        while let Some(cur) = stack.pop() {
            if !seen.insert(cur.clone()) {
                continue;
            }
            for child in self.runtime_children(&cur) {
                if self.nodes.contains_key(&child) && !seen.contains(&child) {
                    stack.push(child);
                }
            }
        }
        seen
    }
}
