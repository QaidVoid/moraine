//! The single resolution entry point: request to resolved solution.

use std::collections::{BTreeMap, BTreeSet};

use moraine_atom::Atom;
use moraine_common::Interner;
use moraine_eapi::{PERMISSIVE, features_for};
use moraine_solver::{Explanation, solve_with_stats};
use moraine_version::Version;
use tracing::instrument;

use crate::depnode::{BlockerKind, DepNode, NormAtom, SlotOpKind};
use crate::encode::{CLASSES, root_for, slot_matches, version_satisfies};
use crate::error::ResolveError;
use crate::normalize::normalize_atom;
use crate::provider::{GentooProvider, REQUEST_CP};
use crate::solution::{
    DepClass, DepEdge, RecordedBlocker, ResolvedPackage, ResolvedSolution, SlotBinding,
};
use crate::source::{PackageMeta, ResolveSource};

/// Resolve a set of request atom strings against the given source, producing a
/// resolved solution or a structured failure.
#[instrument(skip(source), fields(requests = requests.len()))]
pub fn resolve<S: ResolveSource>(
    source: &S,
    requests: &[&str],
) -> Result<ResolvedSolution, ResolveError> {
    let interner = Interner::new();
    let mut request_atoms: Vec<NormAtom> = Vec::with_capacity(requests.len());
    for req in requests {
        let atom =
            Atom::parse(req, PERMISSIVE, &interner).map_err(|e| ResolveError::BadRequest {
                atom: (*req).to_owned(),
                reason: e.to_string(),
            })?;
        request_atoms.push(normalize_atom(&atom, &interner));
    }

    let provider = GentooProvider::with_request(source, request_atoms.clone());
    let root_version = Version::parse("0").expect("synthetic version parses");

    let (solution, stats) = solve_with_stats(&provider, REQUEST_CP.to_owned(), root_version);
    let decisions = match solution {
        Ok(map) => map,
        Err(failure) => {
            return Err(ResolveError::Unsatisfiable {
                explanation: render_explanation(&failure.explanation),
            });
        }
    };

    let mut resolved = assemble_solution(source, &decisions)?;
    resolved.backtracks = stats.backtracks;
    Ok(resolved)
}

/// Build the resolved solution from the solver's `cp -> version` decisions.
fn assemble_solution<S: ResolveSource>(
    source: &S,
    decisions: &BTreeMap<String, Version>,
) -> Result<ResolvedSolution, ResolveError> {
    // The set of selected (cp, version) excluding the synthetic root.
    let mut selected: BTreeMap<String, (Version, PackageMeta)> = BTreeMap::new();
    for (cp, version) in decisions {
        if cp == REQUEST_CP {
            continue;
        }
        if let Some(meta) = source
            .versions_of(cp)
            .into_iter()
            .find(|m| &m.version == version)
        {
            selected.insert(cp.clone(), (version.clone(), meta));
        }
    }

    let mut packages: Vec<ResolvedPackage> = Vec::new();
    let mut edges: Vec<DepEdge> = Vec::new();
    let mut blockers: Vec<RecordedBlocker> = Vec::new();

    for (cp, (version, meta)) in &selected {
        let resolved_use = source.resolved_use(meta);
        let features = features_for(&meta.eapi);
        let already_installed = source.installed_matches(cp, version, &meta.slot);

        // Slot-operator rebuild detection: if this package is installed and
        // recorded a `:=` binding whose provider is now selected with a
        // different sub-slot, the existing build is stale and must be rebuilt.
        let mut subslot_rebuild = false;
        for inst in source.installed(cp) {
            for (dep_cp, bslot, bsub) in &inst.slot_bindings {
                if let Some((_, dep_meta)) = selected.get(dep_cp)
                    && &dep_meta.slot == bslot
                    && &dep_meta.subslot != bsub
                {
                    subslot_rebuild = true;
                }
            }
        }

        let mut slot_bindings: Vec<SlotBinding> = Vec::new();

        let class_nodes: [(&DepNode, DepClass); 5] = [
            (&meta.bdepend, DepClass::Bdepend),
            (&meta.depend, DepClass::Depend),
            (&meta.rdepend, DepClass::Rdepend),
            (&meta.pdepend, DepClass::Pdepend),
            (&meta.idepend, DepClass::Idepend),
        ];

        for (node, class) in class_nodes {
            let mut atoms: Vec<(&NormAtom, bool)> = Vec::new();
            collect_atoms(node, &resolved_use, false, &mut atoms);
            for (atom, optional) in atoms {
                emit_edge_for_atom(
                    source,
                    cp,
                    atom,
                    class,
                    optional,
                    features,
                    &selected,
                    &mut edges,
                    &mut blockers,
                    &mut slot_bindings,
                );
            }
        }

        packages.push(ResolvedPackage {
            cp: cp.clone(),
            version: version.clone(),
            slot: meta.slot.clone(),
            subslot: meta.subslot.clone(),
            use_enabled: resolved_use,
            slot_bindings,
            already_installed,
            subslot_rebuild,
        });
    }

    edges.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then(a.to.cmp(&b.to))
            .then(a.class.cmp(&b.class))
    });
    edges.dedup();
    blockers.sort_by(|a, b| {
        a.blocker
            .cmp(&b.blocker)
            .then(a.blocked_atom.cmp(&b.blocked_atom))
    });
    blockers.dedup();

    Ok(ResolvedSolution {
        packages,
        edges,
        blockers,
        backtracks: 0,
    })
}

/// Collect the live atoms of a dependency node against the parent's USE,
/// tracking whether each came from a `||` branch (optional).
fn collect_atoms<'a>(
    node: &'a DepNode,
    parent_use: &BTreeSet<String>,
    optional: bool,
    out: &mut Vec<(&'a NormAtom, bool)>,
) {
    match node {
        DepNode::Leaf(atom) => out.push((atom, optional)),
        DepNode::AllOf(children) => {
            for c in children {
                collect_atoms(c, parent_use, optional, out);
            }
        }
        DepNode::Conditional { flag, sense, body } => {
            if parent_use.contains(flag) == *sense {
                for c in body {
                    collect_atoms(c, parent_use, optional, out);
                }
            }
        }
        DepNode::AnyOf(branches)
        | DepNode::ExactlyOneOf(branches)
        | DepNode::AtMostOneOf(branches) => {
            for b in branches {
                collect_atoms(b, parent_use, true, out);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_edge_for_atom<S: ResolveSource>(
    source: &S,
    from: &str,
    atom: &NormAtom,
    class: DepClass,
    optional: bool,
    features: moraine_eapi::EapiFeatures,
    selected: &BTreeMap<String, (Version, PackageMeta)>,
    edges: &mut Vec<DepEdge>,
    blockers: &mut Vec<RecordedBlocker>,
    slot_bindings: &mut Vec<SlotBinding>,
) {
    let root = root_for(class, features);

    if atom.blocker != BlockerKind::None {
        blockers.push(RecordedBlocker {
            blocker: from.to_owned(),
            blocked_atom: render_atom(atom),
            strong: atom.blocker == BlockerKind::Strong,
        });
        return;
    }

    // virtual/* atoms: resolve through providers to whichever selected package
    // satisfies them.
    if atom.cp.starts_with("virtual/") {
        emit_virtual_edges(
            source, from, atom, class, optional, features, selected, edges,
        );
        return;
    }

    // Find the selected provider satisfying this atom.
    if let Some((dep_version, dep_meta)) = selected.get(&atom.cp) {
        if !version_satisfies(atom, dep_version) || !slot_matches(atom, dep_meta) {
            return;
        }
        let slot_op = atom.slot_op.is_some();
        edges.push(DepEdge {
            from: from.to_owned(),
            to: atom.cp.clone(),
            class,
            root,
            build_time: class.is_build_time(),
            slot_op,
            optional,
        });
        // Record `:=`/`:slot=` bindings.
        if matches!(atom.slot_op, Some(SlotOpKind::Equal)) {
            slot_bindings.push(SlotBinding {
                dependency: atom.cp.clone(),
                slot: dep_meta.slot.clone(),
                subslot: dep_meta.subslot.clone(),
                root,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_virtual_edges<S: ResolveSource>(
    source: &S,
    from: &str,
    atom: &NormAtom,
    class: DepClass,
    optional: bool,
    features: moraine_eapi::EapiFeatures,
    selected: &BTreeMap<String, (Version, PackageMeta)>,
    edges: &mut Vec<DepEdge>,
) {
    let root = root_for(class, features);
    // Edge to the virtual itself if it is selected.
    if let Some((vv, _)) = selected.get(&atom.cp)
        && version_satisfies(atom, vv)
    {
        edges.push(DepEdge {
            from: from.to_owned(),
            to: atom.cp.clone(),
            class,
            root,
            build_time: class.is_build_time(),
            slot_op: atom.slot_op.is_some(),
            optional,
        });
    }
    // Follow the virtual's RDEPEND to the chosen provider.
    let mut virtuals = source.versions_of(&atom.cp);
    virtuals.sort_by(|a, b| b.version.cmp(&a.version));
    for vmeta in &virtuals {
        if !version_satisfies(atom, &vmeta.version) {
            continue;
        }
        let vuse = source.resolved_use(vmeta);
        let mut patoms: Vec<(&NormAtom, bool)> = Vec::new();
        collect_atoms(&vmeta.rdepend, &vuse, true, &mut patoms);
        for (patom, _) in patoms {
            if patom.cp.starts_with("virtual/") {
                emit_virtual_edges(source, from, patom, class, true, features, selected, edges);
                continue;
            }
            if let Some((dv, dm)) = selected.get(&patom.cp)
                && version_satisfies(patom, dv)
                && slot_matches(patom, dm)
            {
                edges.push(DepEdge {
                    from: from.to_owned(),
                    to: patom.cp.clone(),
                    class,
                    root,
                    build_time: class.is_build_time(),
                    slot_op: patom.slot_op.is_some(),
                    optional: true,
                });
            }
        }
        // Only the highest matching virtual contributes.
        break;
    }
}

/// Render a normalized atom for diagnostics.
fn render_atom(atom: &NormAtom) -> String {
    let mut s = String::new();
    match atom.blocker {
        BlockerKind::None => {}
        BlockerKind::Weak => s.push('!'),
        BlockerKind::Strong => s.push_str("!!"),
    }
    if let Some((op, v)) = &atom.version {
        s.push_str(op_str(*op));
        s.push_str(&atom.cp);
        s.push('-');
        s.push_str(v.as_str());
    } else {
        s.push_str(&atom.cp);
    }
    if let Some(slot) = &atom.slot {
        s.push(':');
        s.push_str(slot);
    }
    s
}

fn op_str(op: crate::depnode::Op) -> &'static str {
    use crate::depnode::Op::*;
    match op {
        Equal | EqualGlob => "=",
        GreaterEqual => ">=",
        LessEqual => "<=",
        Greater => ">",
        Less => "<",
        Tilde => "~",
    }
}

/// Render the solver's explanation tree into an indented string.
fn render_explanation(explanation: &Explanation<String, Version>) -> String {
    let mut out = String::new();
    render_node(explanation, 0, &mut out);
    out
}

fn render_node(node: &Explanation<String, Version>, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    match node {
        Explanation::External {
            description, terms, ..
        } => {
            out.push_str(&format!(
                "{indent}- {description} [{}]\n",
                render_terms(terms)
            ));
        }
        Explanation::Derived { causes, terms, .. } => {
            out.push_str(&format!(
                "{indent}- conflict [{}] derived from:\n",
                render_terms(terms)
            ));
            for c in causes {
                render_node(c, depth + 1, out);
            }
        }
        Explanation::Shared(id) => {
            out.push_str(&format!("{indent}- (see step {id})\n"));
        }
    }
}

fn render_terms(terms: &[(String, moraine_solver::Term<Version>)]) -> String {
    terms
        .iter()
        .map(|(p, _)| p.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

// Keep CLASSES referenced so it is not dead in some build configurations.
const _: [DepClass; 5] = CLASSES;
