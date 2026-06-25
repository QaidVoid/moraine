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

/// A solver disjunction alternative: a target `(cp, slot)` key and the term that
/// constrains its candidate versions.
pub(crate) type Alt = (String, Term<Version>);

/// The package currently being encoded, whose own `(cp, slot)` is excluded from
/// its blocker target sets (the same-slot-replacement exception).
#[derive(Clone, Copy)]
pub(crate) struct Parent<'a> {
    pub cp: &'a str,
    pub slot: &'a str,
}

/// A satisfiable `||` branch reduced for encoding: its declaration index, one
/// alternative list per required atom, and the branch's blocker atoms.
type EncodedBranch<'a> = (usize, Vec<Vec<Alt>>, Vec<&'a NormAtom>);

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
        // `^^`/`??` are REQUIRED_USE-only and never appear in dependency strings;
        // treat them as any-of here defensively.
        DepNode::AnyOf(branches)
        | DepNode::ExactlyOneOf(branches)
        | DepNode::AtMostOneOf(branches) => {
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
            DepNode::AllOf(c)
            | DepNode::AnyOf(c)
            | DepNode::ExactlyOneOf(c)
            | DepNode::AtMostOneOf(c) => {
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
        skip_build: bool,
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

        // The parent's own (cp, slot) is excluded from its blocker target sets so
        // a self/other-slot block constrains only the other slots.
        let parent = Parent {
            cp: meta.cp.as_str(),
            slot: meta.slot.as_str(),
        };

        for (node, class) in class_nodes {
            // An already-built (installed, not rebuilt) package does not need its
            // build-time dependencies pulled into the graph; only its runtime
            // dependencies matter. This also matches a binary install, where the
            // build deps are never required.
            if skip_build && class.is_build_time() {
                continue;
            }
            let mut atoms: Vec<&NormAtom> = Vec::new();
            let mut groups: Vec<Group> = Vec::new();
            reduce(node, parent_use, &mut atoms, &mut groups);

            for atom in atoms {
                self.encode_atom(
                    atom,
                    parent_use,
                    parent,
                    &mut clauses,
                    &mut conflicts,
                    features,
                )?;
            }
            for group in &groups {
                self.encode_group(
                    group,
                    parent_use,
                    parent,
                    &mut clauses,
                    &mut conflicts,
                    features,
                )?;
            }
        }

        Ok(Requirements { clauses, conflicts })
    }

    /// Build the request root's requirements from the request atoms, keyed by
    /// `(cp, slot)` like any other package. A no-provider atom yields a clause the
    /// solver cannot satisfy (a bare `cp` key), so the failure names the `cp`.
    pub fn request_requirements(&self, atoms: &[NormAtom]) -> Requirements<String, Version> {
        let mut clauses: Vec<Clause<String, Version>> = Vec::new();
        let mut conflicts: Vec<(String, Term<Version>)> = Vec::new();
        let parent_use = BTreeSet::new();
        let features = moraine_eapi::PERMISSIVE;
        for atom in atoms {
            if atom.blocker != BlockerKind::None {
                for alt in self.blocked_alternatives(atom, None, &parent_use, features) {
                    conflicts.push(alt);
                }
                continue;
            }
            if atom.cp.starts_with("virtual/") {
                match self.expand_virtual(atom, features) {
                    Some(alts) => push_disjunction(&mut clauses, alts),
                    None => clauses.push(Clause::single(
                        atom.cp.clone(),
                        Term::positive(Range::full()),
                    )),
                }
                continue;
            }
            if self.atom_is_provided(atom) {
                continue;
            }
            let alts = self.required_alternatives(atom, &parent_use, features);
            if alts.is_empty() {
                clauses.push(Clause::single(
                    atom.cp.clone(),
                    Term::positive(Range::full()),
                ));
            } else {
                push_disjunction(&mut clauses, alts);
            }
        }
        Requirements { clauses, conflicts }
    }

    /// Encode one plain (required) atom into a clause or a conflict. Returns
    /// `Err(reason)` if a required atom has no candidate at all.
    fn encode_atom(
        &self,
        atom: &NormAtom,
        parent_use: &BTreeSet<String>,
        parent: Parent<'_>,
        clauses: &mut Vec<Clause<String, Version>>,
        conflicts: &mut Vec<(String, Term<Version>)>,
        features: EapiFeatures,
    ) -> Result<(), String> {
        // Blockers become conflicts, one per blocked `(cp, slot)`, excluding the
        // parent's own instance.
        if atom.blocker != BlockerKind::None {
            for alt in self.blocked_alternatives(atom, Some(parent), parent_use, features) {
                conflicts.push(alt);
            }
            return Ok(());
        }

        // package.provided satisfies the atom with no install.
        if self.atom_is_provided(atom) {
            return Ok(());
        }

        // A `virtual/*` atom is required like any package: selecting the virtual
        // node retains it in the install/world set, and its own RDEPEND (a
        // provider disjunction) is processed when the node is encoded, pulling in
        // a provider. A virtual whose providers are all USE-gated off installs as
        // a node with no provider, which is then trivially satisfied.
        let alts = self.required_alternatives(atom, parent_use, features);
        if alts.is_empty() {
            // No visible virtual node: fall back to flattening providers so a
            // provider can still be pulled in directly.
            if atom.cp.starts_with("virtual/")
                && let Some(provider_alts) = self.expand_virtual(atom, features)
            {
                push_disjunction(clauses, provider_alts);
                return Ok(());
            }
            return Err(format!("no provider for {}", atom.cp));
        }
        push_disjunction(clauses, alts);
        Ok(())
    }

    /// Encode a `||` any-of group.
    ///
    /// Each satisfiable branch is reduced to its full atom list (every required
    /// atom's `(cp, slot)` alternatives, plus its blocker atoms), mirroring
    /// `dep_zapdeps` returning the entire selected branch rather than only its
    /// first atom. Overlapping branches are reordered so the branch adding the
    /// fewest new slots is preferred. The chosen (first, after ordering) branch
    /// is emitted as a conjunction of all its atoms (so `|| ( ( a b ) c )` pulls
    /// in both `a` and `b`), and its blockers are asserted. An any-of over every
    /// satisfiable branch's leading atom is also emitted so the solver can fall
    /// back to another branch on a learned conflict.
    fn encode_group(
        &self,
        group: &Group,
        parent_use: &BTreeSet<String>,
        parent: Parent<'_>,
        clauses: &mut Vec<Clause<String, Version>>,
        conflicts: &mut Vec<(String, Term<Version>)>,
        features: EapiFeatures,
    ) -> Result<(), String> {
        // Each satisfiable branch: its index, the per-atom alternative lists (one
        // disjunction per required atom), and its blocker atoms.
        let mut branches: Vec<EncodedBranch> = Vec::new();
        for (idx, branch) in group.iter().enumerate() {
            let mut atom_alts: Vec<Vec<Alt>> = Vec::new();
            let mut blockers: Vec<&NormAtom> = Vec::new();
            let mut satisfiable = true;
            let mut any_required = false;
            for atom in branch {
                if atom.blocker != BlockerKind::None {
                    blockers.push(atom);
                    continue;
                }
                if self.atom_is_provided(atom) {
                    continue;
                }
                any_required = true;
                let alts = if atom.cp.starts_with("virtual/") {
                    self.expand_virtual(atom, features)
                } else {
                    let a = self.required_alternatives(atom, parent_use, features);
                    if a.is_empty() { None } else { Some(a) }
                };
                match alts {
                    Some(a) => atom_alts.push(a),
                    None => {
                        satisfiable = false;
                        break;
                    }
                }
            }
            // A branch with no required atom (all provided, or pure blockers)
            // satisfies the group with no install.
            if satisfiable && !any_required {
                return Ok(());
            }
            if satisfiable {
                branches.push((idx, atom_alts, blockers));
            }
        }

        if branches.is_empty() {
            return Err("no satisfiable branch in any-of group".to_owned());
        }

        // PMS || resolution: prefer a branch already fully satisfied by the
        // installed set. When the branches are version windows of one package
        // (cabal-style `|| ( <X >=Y )`), prefer the highest version next, like
        // Portage; otherwise keep the leftmost (so an installed member such as
        // gentoo-kernel-bin still wins and Portage's default order holds).
        let same_cp = {
            let mut lead = branches
                .iter()
                .filter_map(|(idx, _, _)| Self::branch_lead_cp(&group[*idx]));
            match lead.next() {
                Some(first) => lead.all(|cp| cp == first),
                None => false,
            }
        };
        let best: std::collections::HashMap<usize, Option<Version>> = if same_cp {
            branches
                .iter()
                .map(|(idx, _, _)| (*idx, self.branch_best_version(&group[*idx])))
                .collect()
        } else {
            std::collections::HashMap::new()
        };
        branches.sort_by(|a, b| {
            self.branch_needs_new(&a.1)
                .cmp(&self.branch_needs_new(&b.1))
                .then_with(|| {
                    if same_cp {
                        best[&b.0].cmp(&best[&a.0]).then(a.0.cmp(&b.0))
                    } else {
                        a.0.cmp(&b.0)
                    }
                })
        });

        // The chosen branch is the first after ordering: emit its full
        // conjunction (every atom required) and assert its blockers.
        let (_, chosen_atoms, chosen_blockers) = &branches[0];
        for alts in chosen_atoms {
            push_disjunction(clauses, alts.clone());
        }
        for blocker in chosen_blockers {
            for alt in self.blocked_alternatives(blocker, Some(parent), parent_use, features) {
                conflicts.push(alt);
            }
        }

        // Any-of fallback over every satisfiable branch's leading atom, so the
        // solver can switch branches when the chosen one hits a learned conflict.
        let leaders: Vec<Alt> = branches
            .iter()
            .filter_map(|(_, atoms, _)| atoms.first())
            .flat_map(|alts| alts.iter().cloned())
            .collect();
        if leaders.len() > 1 {
            clauses.push(Clause::any_of(leaders));
        }
        Ok(())
    }

    /// Whether a branch requires installing at least one package not already
    /// present, used to prefer `||` branches fully met by the installed set. A
    /// branch is "free" only when every required atom has an installed provider.
    fn branch_needs_new(&self, atom_alts: &[Vec<Alt>]) -> bool {
        atom_alts.iter().any(|alts| {
            !alts.iter().any(|(key, _)| {
                let cp = crate::provider::split_key(key).map_or(key.as_str(), |(c, _)| c);
                !self.source.installed(cp).is_empty()
            })
        })
    }

    /// The category/package of a branch's first required (non-blocker) atom.
    fn branch_lead_cp<'b>(branch: &'b [&NormAtom]) -> Option<&'b str> {
        branch
            .iter()
            .find(|a| a.blocker == BlockerKind::None)
            .map(|a| a.cp.as_str())
    }

    /// The highest selectable version satisfying every same-`cp` atom of a
    /// branch, used to prefer the newest window of a same-package `||` version
    /// range (cabal-style `|| ( <X >=Y )`), as Portage does.
    fn branch_best_version(&self, branch: &[&NormAtom]) -> Option<Version> {
        let cp = Self::branch_lead_cp(branch)?;
        self.source
            .versions_of(cp)
            .into_iter()
            .filter(|m| {
                branch
                    .iter()
                    .filter(|a| a.blocker == BlockerKind::None && a.cp == cp)
                    .all(|a| version_satisfies(a, &m.version) && slot_matches(a, m))
            })
            .filter(|m| {
                self.source.is_visible(m) || self.source.acceptability(m).is_autounmaskable()
            })
            .map(|m| m.version)
            .max()
    }

    /// The required atom's matching visible versions grouped into one `(cp,
    /// slot)` alternative per slot, ordered installed-slot first then by highest
    /// version, so a slotless atom becomes a disjunction over its available slots
    /// and a slotted atom maps to its single slot variable.
    fn required_alternatives(
        &self,
        atom: &NormAtom,
        parent_use: &BTreeSet<String>,
        features: EapiFeatures,
    ) -> Vec<Alt> {
        let mut by_slot: std::collections::BTreeMap<String, Vec<Version>> =
            std::collections::BTreeMap::new();
        for m in self.source.versions_of(&atom.cp) {
            if !version_satisfies(atom, &m.version) || !slot_matches(atom, &m) {
                continue;
            }
            if !self.source.is_visible(&m) {
                continue;
            }
            let cand_use = self.source.resolved_use(&m);
            if !use_deps_satisfied(atom, &cand_use, &m.iuse, parent_use, features) {
                continue;
            }
            by_slot.entry(m.slot.clone()).or_default().push(m.version);
        }
        if by_slot.is_empty() {
            // Autounmask: no visible version satisfies the atom, so admit
            // soft-masked (keyword/license) versions as alternatives. The change
            // is reported after resolution; hard masks stay excluded.
            for m in self.source.versions_of(&atom.cp) {
                if !version_satisfies(atom, &m.version) || !slot_matches(atom, &m) {
                    continue;
                }
                if !self.source.acceptability(&m).is_autounmaskable() {
                    continue;
                }
                let cand_use = self.source.resolved_use(&m);
                if !use_deps_satisfied(atom, &cand_use, &m.iuse, parent_use, features) {
                    continue;
                }
                by_slot.entry(m.slot.clone()).or_default().push(m.version);
            }
        }
        self.to_alternatives(&atom.cp, by_slot)
    }

    /// The blocker atom's matching versions (repository and installed) grouped
    /// into one `(cp, slot)` alternative per slot, excluding the parent's own
    /// instance so a self/other-slot block constrains only the other instance.
    fn blocked_alternatives(
        &self,
        atom: &NormAtom,
        parent: Option<Parent<'_>>,
        parent_use: &BTreeSet<String>,
        features: EapiFeatures,
    ) -> Vec<Alt> {
        // A blocker never constrains the parent's own slot: in that slot the
        // parent's install replaces the matched package rather than coexisting
        // with it (the same-slot-replacement exception), so a self/other-slot
        // block constrains only the other slots.
        let excluded =
            |slot: &str| -> bool { matches!(parent, Some(p) if p.cp == atom.cp && p.slot == slot) };
        let mut by_slot: std::collections::BTreeMap<String, Vec<Version>> =
            std::collections::BTreeMap::new();
        for m in self.source.versions_of(&atom.cp) {
            if version_satisfies(atom, &m.version)
                && slot_matches(atom, &m)
                && use_deps_satisfied(
                    atom,
                    &self.source.resolved_use(&m),
                    &m.iuse,
                    parent_use,
                    features,
                )
                && !excluded(&m.slot)
            {
                by_slot.entry(m.slot.clone()).or_default().push(m.version);
            }
        }
        for m in self.source.installed(&atom.cp) {
            // An installed package's USE is fixed (its recorded enabled set), but
            // its declared IUSE must be used for the blocker's USE condition so a
            // `[flag(+)]` default is not wrongly applied to a declared-but-disabled
            // flag. A slotted blocker (`!pkg:slot`) does not match a different slot.
            let slot_ok = atom.slot.as_ref().is_none_or(|s| &m.slot == s);
            if slot_ok
                && version_satisfies(atom, &m.version)
                && use_deps_satisfied(atom, &m.use_enabled, &m.iuse, parent_use, features)
                && !excluded(&m.slot)
            {
                by_slot.entry(m.slot.clone()).or_default().push(m.version);
            }
        }
        self.to_alternatives(&atom.cp, by_slot)
    }

    /// Turn a slot-to-versions map into ordered `(cp, slot)` alternatives:
    /// installed slots first, then by highest version.
    fn to_alternatives(
        &self,
        cp: &str,
        by_slot: std::collections::BTreeMap<String, Vec<Version>>,
    ) -> Vec<Alt> {
        let installed_slots: BTreeSet<String> = self
            .source
            .installed(cp)
            .iter()
            .map(|i| i.slot.clone())
            .collect();
        let mut slots: Vec<(String, Vec<Version>)> = by_slot.into_iter().collect();
        slots.sort_by(|a, b| {
            let ai = installed_slots.contains(&a.0);
            let bi = installed_slots.contains(&b.0);
            bi.cmp(&ai)
                .then_with(|| b.1.iter().max().cmp(&a.1.iter().max()))
        });
        slots
            .into_iter()
            .map(|(slot, mut versions)| {
                versions.sort();
                versions.dedup();
                (
                    crate::provider::package_key(cp, &slot),
                    Term::positive(versions_to_range(&versions)),
                )
            })
            .collect()
    }

    /// Expand a `virtual/*` atom into provider alternatives, highest virtual
    /// version first, following only RDEPEND. Each provider dependency is
    /// evaluated against the virtual's own USE.
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
                if pa.cp.starts_with("virtual/") {
                    if let Some(nested) = self.expand_virtual(pa, features) {
                        providers.extend(nested);
                    }
                    continue;
                }
                if self.atom_is_provided(pa) {
                    continue;
                }
                providers.extend(self.required_alternatives(pa, &vuse, features));
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

    fn atom_with_use_dep(flag: &str, default: Option<bool>) -> NormAtom {
        NormAtom {
            blocker: crate::depnode::BlockerKind::None,
            cp: "sys-libs/glibc".to_owned(),
            version: None,
            slot: None,
            subslot: None,
            slot_op: None,
            use_deps: vec![crate::depnode::UseReq {
                flag: flag.to_owned(),
                kind: crate::depnode::UseReqKind::Enabled,
                default,
            }],
        }
    }

    #[test]
    fn use_dep_default_yields_to_declared_disabled_flag() {
        // `[vanilla(+)]` must not be assumed enabled when the candidate actually
        // declares `vanilla` (in IUSE) but has it disabled: the declared state
        // wins over the `(+)` default. This guards the installed-blocker path.
        let atom = atom_with_use_dep("vanilla", Some(true));
        let parent = BTreeSet::new();
        let f8 = features_for("8");

        let iuse: BTreeSet<String> = ["vanilla".to_owned()].into_iter().collect();
        let disabled = BTreeSet::new();
        assert!(!use_deps_satisfied(&atom, &disabled, &iuse, &parent, f8));

        // With `vanilla` genuinely absent from IUSE the `(+)` default applies.
        let no_iuse = BTreeSet::new();
        assert!(use_deps_satisfied(&atom, &disabled, &no_iuse, &parent, f8));
    }
}
