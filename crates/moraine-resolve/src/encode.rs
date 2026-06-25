//! Translation of normalized Gentoo dependency ASTs into solver requirements.
//!
//! The encoder reduces USE-conditional groups against the package's resolved
//! USE, turns each plain atom into a one-alternative clause, each `||` group
//! into a multi-alternative clause, expands `virtual/*` atoms into a disjunction
//! over providers, and turns blockers into conflicts. It produces solver
//! `Requirements` for the provider and, after resolution, the class-tagged edge
//! records for the solution.

use std::collections::BTreeSet;
use std::ops::Bound;

use moraine_eapi::EapiFeatures;
use moraine_solver::{Clause, Range, Requirements, Term};
use moraine_version::Version;

use crate::depnode::{BlockerKind, DepNode, NormAtom, Op, SlotOpKind, UseReqKind};
use crate::error::ResolveError;
use crate::solution::{DepClass, Root};
use crate::source::ResolveSource;

/// The five dependency classes, each tagged with how its `:=` bindings resolve.
pub(crate) const CLASSES: [DepClass; 5] = [
    DepClass::Bdepend,
    DepClass::Depend,
    DepClass::Rdepend,
    DepClass::Pdepend,
    DepClass::Idepend,
];

/// Resolve the target root for a dependency class, gating DEPEND on the EAPI
/// bdepend feature.
pub(crate) fn root_for(class: DepClass, features: EapiFeatures) -> Root {
    match class {
        DepClass::Bdepend => Root::BuildHost,
        DepClass::Depend => {
            if features.bdepend {
                Root::TargetSysroot
            } else {
                Root::BuildHost
            }
        }
        DepClass::Rdepend | DepClass::Pdepend | DepClass::Idepend => Root::Target,
    }
}

/// Whether a concrete candidate version satisfies a normalized atom's version
/// constraint, applying the exact semantics the coarse range cannot express.
pub(crate) fn version_satisfies(atom: &NormAtom, candidate: &Version) -> bool {
    match &atom.version {
        None => true,
        Some((op, v)) => match op {
            Op::Equal => candidate == v,
            Op::GreaterEqual => candidate >= v,
            Op::Greater => candidate > v,
            Op::LessEqual => candidate <= v,
            Op::Less => candidate < v,
            Op::Tilde => candidate.matches_any_revision(v),
            Op::EqualGlob => candidate.as_str().starts_with(v.as_str()),
        },
    }
}

/// Whether a candidate's resolved USE satisfies an atom's USE-dependency
/// requirements, given the parent's resolved USE and the candidate's IUSE.
pub(crate) fn use_deps_satisfied(
    atom: &NormAtom,
    candidate_use: &BTreeSet<String>,
    candidate_iuse: &BTreeSet<String>,
    parent_use: &BTreeSet<String>,
    features: EapiFeatures,
) -> bool {
    for dep in &atom.use_deps {
        // Determine the candidate's state for the flag, honoring defaults.
        let known = candidate_iuse.contains(&dep.flag);
        let enabled = if known {
            candidate_use.contains(&dep.flag)
        } else if features.use_dep_defaults {
            match dep.default {
                Some(default) => default,
                None => return false,
            }
        } else {
            return false;
        };
        let parent_enabled = parent_use.contains(&dep.flag);
        let ok = match dep.kind {
            UseReqKind::Enabled => enabled,
            UseReqKind::Disabled => !enabled,
            UseReqKind::EnabledIfParent => !parent_enabled || enabled,
            UseReqKind::DisabledIfParent => parent_enabled || !enabled,
            UseReqKind::EqualToParent => enabled == parent_enabled,
            UseReqKind::OppositeToParent => enabled != parent_enabled,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// An any-of group reduced against USE: a list of branches, each branch a
/// conjunction of atoms.
pub(crate) type Group<'a> = Vec<Vec<&'a NormAtom>>;

/// A solver disjunction alternative: a target package and the term that
/// constrains its candidates.
pub(crate) type Alt = (String, Term<Version>);

/// Recursively reduce a dependency node against the parent's USE, collecting the
/// live top-level atoms and the any-of groups (with branch structure preserved).
fn reduce<'a>(
    node: &'a DepNode,
    parent_use: &BTreeSet<String>,
    out_atoms: &mut Vec<&'a NormAtom>,
    out_groups: &mut Vec<Group<'a>>,
) {
    match node {
        DepNode::Leaf(atom) => out_atoms.push(atom),
        DepNode::AllOf(children) => {
            for c in children {
                reduce(c, parent_use, out_atoms, out_groups);
            }
        }
        DepNode::Conditional { flag, sense, body } => {
            let live = parent_use.contains(flag) == *sense;
            if live {
                for c in body {
                    reduce(c, parent_use, out_atoms, out_groups);
                }
            }
        }
        DepNode::AnyOf(branches) => {
            // Each branch reduces to its own conjunction of atoms (nested groups
            // inside a branch are flattened into the branch's atom list, which is
            // an acceptable approximation for the common corpus).
            let mut group: Group<'a> = Vec::new();
            for b in branches {
                let mut atoms: Vec<&NormAtom> = Vec::new();
                let mut nested: Vec<Group<'a>> = Vec::new();
                reduce(b, parent_use, &mut atoms, &mut nested);
                for g in nested {
                    for branch in g {
                        atoms.extend(branch);
                    }
                }
                if !atoms.is_empty() {
                    group.push(atoms);
                }
            }
            if !group.is_empty() {
                out_groups.push(group);
            }
        }
    }
}

/// The encoder over a single package's metadata and resolved USE.
pub(crate) struct Encoder<'s, S: ResolveSource> {
    pub source: &'s S,
}

impl<'s, S: ResolveSource> Encoder<'s, S> {
    /// Validate that a package's dependency strings do not use EAPI features
    /// their EAPI lacks (the only check is strong blockers here, since the rest
    /// is enforced at parse time by `moraine-atom`).
    pub fn validate_blockers(
        &self,
        cp: &str,
        nodes: &[&DepNode],
        features: EapiFeatures,
    ) -> Result<(), ResolveError> {
        for node in nodes {
            self.validate_node(cp, node, features)?;
        }
        Ok(())
    }

    fn validate_node(
        &self,
        cp: &str,
        node: &DepNode,
        features: EapiFeatures,
    ) -> Result<(), ResolveError> {
        match node {
            DepNode::Leaf(atom) => {
                if atom.blocker == BlockerKind::Strong && !features.strong_blocks {
                    return Err(ResolveError::InvalidDependency {
                        package: cp.to_owned(),
                        reason: format!(
                            "strong blocker !!{} requires an EAPI with strong-block support",
                            atom.cp
                        ),
                    });
                }
                Ok(())
            }
            DepNode::AllOf(c) | DepNode::AnyOf(c) => {
                for n in c {
                    self.validate_node(cp, n, features)?;
                }
                Ok(())
            }
            DepNode::Conditional { body, .. } => {
                for n in body {
                    self.validate_node(cp, n, features)?;
                }
                Ok(())
            }
        }
    }

    /// Build the solver requirements for a package version with the given
    /// resolved USE and EAPI features. Returns `Err(reason)` when a required
    /// dependency has no candidate at all, which makes this version unusable and
    /// is surfaced as `Dependencies::Unavailable`.
    pub fn requirements(
        &self,
        meta: &crate::source::PackageMeta,
        parent_use: &BTreeSet<String>,
        features: EapiFeatures,
    ) -> Result<Requirements<String, Version>, String> {
        let mut clauses: Vec<Clause<String, Version>> = Vec::new();
        let mut conflicts: Vec<(String, Term<Version>)> = Vec::new();

        let class_nodes: [(&DepNode, DepClass); 5] = [
            (&meta.bdepend, DepClass::Bdepend),
            (&meta.depend, DepClass::Depend),
            (&meta.rdepend, DepClass::Rdepend),
            (&meta.pdepend, DepClass::Pdepend),
            (&meta.idepend, DepClass::Idepend),
        ];

        for (node, _class) in class_nodes {
            let mut atoms: Vec<&NormAtom> = Vec::new();
            let mut groups: Vec<Group> = Vec::new();
            reduce(node, parent_use, &mut atoms, &mut groups);

            for atom in atoms {
                self.encode_atom(atom, parent_use, &mut clauses, &mut conflicts, features)?;
            }
            for group in &groups {
                self.encode_group(group, parent_use, &mut clauses, features)?;
            }
        }

        Ok(Requirements { clauses, conflicts })
    }

    /// Encode one plain (required) atom into a clause or a conflict. Returns
    /// `Err(reason)` if a required atom has no candidate at all.
    fn encode_atom(
        &self,
        atom: &NormAtom,
        parent_use: &BTreeSet<String>,
        clauses: &mut Vec<Clause<String, Version>>,
        conflicts: &mut Vec<(String, Term<Version>)>,
        features: EapiFeatures,
    ) -> Result<(), String> {
        // Blockers become conflicts.
        if atom.blocker != BlockerKind::None {
            if let Some(term) = self.blocked_term(atom) {
                conflicts.push((atom.cp.clone(), term));
            }
            return Ok(());
        }

        // virtual/* atoms expand to a disjunction over providers.
        if atom.cp.starts_with("virtual/") {
            match self.expand_virtual(atom, features) {
                Some(alts) => push_disjunction(clauses, alts),
                None => return Err(format!("no provider for {}", atom.cp)),
            }
            return Ok(());
        }

        // package.provided satisfies the atom with no install.
        if self.atom_is_provided(atom) {
            return Ok(());
        }

        match self.required_term(atom, parent_use, features) {
            Some(term) => {
                clauses.push(Clause::single(atom.cp.clone(), term));
                Ok(())
            }
            None => Err(format!("no candidate satisfies {}", atom.cp)),
        }
    }

    /// Encode a `||` any-of group into a disjunction clause.
    ///
    /// Branches whose category/package sets overlap are reordered so that the
    /// branch adding the fewest new slots is preferred (a lightweight form of
    /// the DNF preference Portage applies only to overlapping groups, avoiding
    /// the exponential blow-up of unconditional DNF). Non-overlapping branches
    /// stay a plain disjunction in declaration order. Each branch contributes
    /// its first satisfiable atom as its representative alternative.
    fn encode_group(
        &self,
        group: &Group,
        parent_use: &BTreeSet<String>,
        clauses: &mut Vec<Clause<String, Version>>,
        features: EapiFeatures,
    ) -> Result<(), String> {
        // Resolve each branch to its representative alternatives, dropping
        // branches that cannot be satisfied.
        let overlapping = branches_overlap(group);
        let mut branch_reps: Vec<(usize, Vec<Alt>)> = Vec::new();
        for (idx, branch) in group.iter().enumerate() {
            let mut reps: Vec<Alt> = Vec::new();
            let mut satisfiable = true;
            for atom in branch {
                if atom.blocker != BlockerKind::None {
                    continue;
                }
                if self.atom_is_provided(atom) {
                    continue;
                }
                if atom.cp.starts_with("virtual/") {
                    match self.expand_virtual(atom, features) {
                        Some(alts) => reps.extend(alts),
                        None => {
                            satisfiable = false;
                            break;
                        }
                    }
                    continue;
                }
                match self.required_term(atom, parent_use, features) {
                    Some(term) => reps.push((atom.cp.clone(), term)),
                    None => {
                        satisfiable = false;
                        break;
                    }
                }
            }
            // A branch whose atoms are all provided satisfies the group with no
            // install.
            if satisfiable && reps.is_empty() {
                return Ok(());
            }
            if satisfiable {
                branch_reps.push((idx, reps));
            }
        }

        if branch_reps.is_empty() {
            return Err("no satisfiable branch in any-of group".to_owned());
        }

        if overlapping {
            // Prefer the branch adding the fewest new slots.
            branch_reps.sort_by(|a, b| {
                self.new_slot_count(&a.1)
                    .cmp(&self.new_slot_count(&b.1))
                    .then(a.0.cmp(&b.0))
            });
        }

        // The disjunction over each branch's leading representative atom.
        let mut alternatives: Vec<Alt> = Vec::new();
        for (_, reps) in &branch_reps {
            if let Some(first) = reps.first() {
                alternatives.push(first.clone());
            }
        }
        push_disjunction(clauses, alternatives);
        Ok(())
    }

    /// The number of providers in a set of alternatives that are not already
    /// installed, used to prefer DNF branches that add the fewest new slots.
    fn new_slot_count(&self, reps: &[Alt]) -> usize {
        reps.iter()
            .filter(|(cp, _)| self.source.installed(cp).is_empty())
            .count()
    }

    /// The positive term constraining a required atom's candidates, narrowed so
    /// only USE-compatible and slot-compatible versions remain. Returns `None`
    /// when no version satisfies the atom plus its USE/slot constraints.
    fn required_term(
        &self,
        atom: &NormAtom,
        parent_use: &BTreeSet<String>,
        features: EapiFeatures,
    ) -> Option<Term<Version>> {
        let allowed = self.matching_versions(atom, parent_use, features);
        if allowed.is_empty() {
            return None;
        }
        Some(Term::positive(versions_to_range(&allowed)))
    }

    /// The negative term for a blocker: the set of versions that must NOT be
    /// selected.
    fn blocked_term(&self, atom: &NormAtom) -> Option<Term<Version>> {
        // Find the versions of the blocked cp that match the blocker atom.
        let blocked = self.matching_versions_simple(atom);
        if blocked.is_empty() {
            return None;
        }
        // The conflict term: the dependency must NOT be in this set.
        Some(Term::positive(versions_to_range(&blocked)))
    }

    /// Public wrapper used for request atoms, applying permissive EAPI features.
    pub fn required_term_pub(
        &self,
        atom: &NormAtom,
        parent_use: &BTreeSet<String>,
    ) -> Option<Term<Version>> {
        self.required_term(atom, parent_use, moraine_eapi::PERMISSIVE)
    }

    /// Public wrapper used for request atoms, applying permissive EAPI features.
    pub fn expand_virtual_pub(&self, atom: &NormAtom) -> Option<Vec<Alt>> {
        self.expand_virtual(atom, moraine_eapi::PERMISSIVE)
    }

    /// Expand a `virtual/*` atom into provider alternatives, highest virtual
    /// version first, following only RDEPEND. A provider dependency is evaluated
    /// against the virtual's own USE (resolved per virtual version below), not
    /// the outer package that pulled the virtual in, so the outer USE is not a
    /// parameter.
    fn expand_virtual(&self, atom: &NormAtom, features: EapiFeatures) -> Option<Vec<Alt>> {
        let mut providers: Vec<Alt> = Vec::new();
        let mut virtuals = self.source.versions_of(&atom.cp);
        // Highest virtual version first.
        virtuals.sort_by(|a, b| b.version.cmp(&a.version));
        for vmeta in &virtuals {
            if !version_satisfies(atom, &vmeta.version) {
                continue;
            }
            if !self.source.is_visible(vmeta) {
                continue;
            }
            let vuse = self.source.resolved_use(vmeta);
            // Follow only the virtual's RDEPEND.
            let mut atoms: Vec<&NormAtom> = Vec::new();
            let mut groups: Vec<Group> = Vec::new();
            reduce(&vmeta.rdepend, &vuse, &mut atoms, &mut groups);
            let mut collected: Vec<&NormAtom> = atoms;
            for g in groups {
                for branch in g {
                    collected.extend(branch);
                }
            }
            for pa in collected {
                // A provider dependency is parented by the virtual, so its USE
                // dependencies (`flag=`, `flag?`, ...) are evaluated against the
                // virtual's own USE, not the outer package that pulled the
                // virtual in.
                if pa.cp.starts_with("virtual/") {
                    if let Some(nested) = self.expand_virtual(pa, features) {
                        providers.extend(nested);
                    }
                    continue;
                }
                if self.atom_is_provided(pa) {
                    continue;
                }
                if let Some(term) = self.required_term(pa, &vuse, features) {
                    providers.push((pa.cp.clone(), term));
                }
            }
        }
        if providers.is_empty() {
            None
        } else {
            Some(providers)
        }
    }

    /// Whether a package.provided entry satisfies the atom.
    fn atom_is_provided(&self, atom: &NormAtom) -> bool {
        // Check every candidate version of the atom against package.provided.
        self.source.versions_of(&atom.cp).iter().any(|m| {
            version_satisfies(atom, &m.version) && self.source.is_provided(&atom.cp, &m.version)
        })
    }

    /// The visible versions of an atom's cp that satisfy its version, slot, and
    /// USE constraints, in ascending order.
    fn matching_versions(
        &self,
        atom: &NormAtom,
        parent_use: &BTreeSet<String>,
        features: EapiFeatures,
    ) -> Vec<Version> {
        let mut out = Vec::new();
        for m in self.source.versions_of(&atom.cp) {
            if !version_satisfies(atom, &m.version) {
                continue;
            }
            if !slot_matches(atom, &m) {
                continue;
            }
            if !self.source.is_visible(&m) {
                continue;
            }
            let cand_use = self.source.resolved_use(&m);
            if !use_deps_satisfied(atom, &cand_use, &m.iuse, parent_use, features) {
                continue;
            }
            out.push(m.version.clone());
        }
        out.sort();
        out.dedup();
        out
    }

    /// The versions of an atom's cp that match version and slot, ignoring
    /// visibility and USE (used for blocker target sets).
    fn matching_versions_simple(&self, atom: &NormAtom) -> Vec<Version> {
        let mut out = Vec::new();
        for m in self.source.versions_of(&atom.cp) {
            if version_satisfies(atom, &m.version) && slot_matches(atom, &m) {
                out.push(m.version.clone());
            }
        }
        for m in self.source.installed(&atom.cp) {
            if version_satisfies(atom, &m.version) {
                out.push(m.version.clone());
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

/// Push a disjunction over alternatives, honoring Portage's greedy
/// preference-order selection.
///
/// The generic solver only decides packages that are positively required, so a
/// bare disjunction (whose alternatives appear only as negative terms) would
/// never drive a selection on its own. Following `dep_zapdeps`, which picks the
/// first satisfiable branch greedily, the encoder emits the most-preferred
/// alternative as a positive requirement so it is selected, and also emits the
/// full disjunction so the solver can fall back to another alternative when the
/// preferred one is contradicted by a learned conflict.
///
/// Known limitation: multi-branch backtracking across `||` branches that each
/// pull disjoint sub-trees collapses to the first branch; this matches Portage's
/// greedy behavior for the common case and is revisited if the corpus demands
/// deeper exploration.
fn push_disjunction(clauses: &mut Vec<Clause<String, Version>>, alternatives: Vec<Alt>) {
    if let Some((cp, term)) = alternatives.first() {
        clauses.push(Clause::single(cp.clone(), term.clone()));
    }
    if alternatives.len() > 1 {
        clauses.push(Clause::any_of(alternatives));
    }
}

/// Whether any two branches of an any-of group share a category/package, which
/// is Portage's condition for applying DNF to the group.
fn branches_overlap(group: &Group) -> bool {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for branch in group {
        let mut local: BTreeSet<&str> = BTreeSet::new();
        for atom in branch {
            local.insert(atom.cp.as_str());
        }
        for cp in &local {
            if seen.contains(cp) {
                return true;
            }
        }
        seen.extend(local);
    }
    false
}

/// Whether a candidate's slot satisfies an atom's slot constraints.
pub(crate) fn slot_matches(atom: &NormAtom, meta: &crate::source::PackageMeta) -> bool {
    match (&atom.slot, atom.slot_op) {
        // `:*` and `:=` (without an explicit slot) match any slot.
        (None, Some(SlotOpKind::Star)) | (None, Some(SlotOpKind::Equal)) | (None, None) => true,
        // `:slot` or `:slot=` requires the exact slot.
        (Some(s), _) => &meta.slot == s,
    }
}

/// Build a solver range that is the union of the singleton ranges of the given
/// versions, so that exactly those versions are admitted.
pub(crate) fn versions_to_range(versions: &[Version]) -> Range<Version> {
    let mut range = Range::empty();
    for v in versions {
        range = range.union(&Range::interval(
            Bound::Included(v.clone()),
            Bound::Included(v.clone()),
        ));
    }
    range
}

#[cfg(test)]
mod tests {
    use super::*;
    use moraine_eapi::{features_for, features_for_level};

    #[test]
    fn class_to_root_mapping() {
        // EAPI 8 has bdepend: DEPEND targets the sysroot.
        let f8 = features_for("8");
        assert_eq!(root_for(DepClass::Bdepend, f8), Root::BuildHost);
        assert_eq!(root_for(DepClass::Depend, f8), Root::TargetSysroot);
        assert_eq!(root_for(DepClass::Rdepend, f8), Root::Target);
        assert_eq!(root_for(DepClass::Pdepend, f8), Root::Target);
        assert_eq!(root_for(DepClass::Idepend, f8), Root::Target);

        // EAPI 6 lacks bdepend: DEPEND targets the running root.
        let f6 = features_for("6");
        assert_eq!(root_for(DepClass::Depend, f6), Root::BuildHost);

        // Level helper agrees.
        assert!(features_for_level(8).bdepend);
        assert!(!features_for_level(6).bdepend);
    }

    #[test]
    fn build_time_classification() {
        assert!(DepClass::Bdepend.is_build_time());
        assert!(DepClass::Depend.is_build_time());
        assert!(DepClass::Rdepend.is_runtime());
        assert!(DepClass::Pdepend.is_runtime());
        assert!(DepClass::Idepend.is_runtime());
    }
}
