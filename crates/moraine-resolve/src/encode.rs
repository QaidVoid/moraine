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

use std::collections::BTreeMap;

use crate::depnode::{BlockerKind, DepNode, NormAtom, Op, SlotOpKind, UseReqKind};
use crate::error::ResolveError;
use crate::solution::{DepClass, Root};
use crate::source::{PackageMeta, ResolveSource, UseChange};

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
            Op::EqualGlob => moraine_atom::version_glob_matches(candidate.as_str(), v.as_str()),
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

/// A branch of an any-of group: its plain (top-level) atoms, all required
/// together, plus any nested any-of groups, each of which contributes one
/// satisfied alternative when the branch is selected.
pub(crate) struct Branch<'a> {
    /// The branch's top-level atoms, required as a conjunction.
    pub atoms: Vec<&'a NormAtom>,
    /// Nested any-of groups inside the branch, preserved as their own
    /// disjunctions rather than flattened into the branch's conjunction.
    pub nested: Vec<Group<'a>>,
}

/// An any-of group reduced against USE: a list of branches.
pub(crate) type Group<'a> = Vec<Branch<'a>>;

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
            // Each branch reduces to its top-level atoms plus any nested any-of
            // groups. A nested `||` group inside a branch is kept as its own
            // disjunction (mirroring `dep_zapdeps` recursing into nested lists)
            // rather than flattened into the branch's conjunction.
            let mut group: Group<'a> = Vec::new();
            for b in branches {
                let mut atoms: Vec<&NormAtom> = Vec::new();
                let mut nested: Vec<Group<'a>> = Vec::new();
                reduce(b, parent_use, &mut atoms, &mut nested);
                if !atoms.is_empty() || !nested.is_empty() {
                    group.push(Branch { atoms, nested });
                }
            }
            if !group.is_empty() {
                out_groups.push(group);
            }
        }
    }
}

/// A recorded `||` any-of branch decision, used by the resolve layer to switch
/// branches on a downstream conflict. The generic solver cannot relax the forced
/// branch on its own, so the resolve layer masks the chosen branch's leader keys
/// and re-encodes, forcing the next branch.
#[derive(Debug, Clone)]
pub(crate) struct BranchPoint {
    /// The solver keys of the chosen branch's leading alternative. Masking these
    /// drops the chosen branch from the next encoding so the next branch wins.
    pub chosen_leader_keys: BTreeSet<String>,
    /// Whether another non-masked branch is available to fall back to.
    pub has_fallback: bool,
}

/// A USE-autounmask proposal discovered while encoding: the target `cp`, its
/// proposed enabled USE, and the per-flag toggles to record as a change.
#[derive(Debug, Clone)]
pub(crate) struct UseProposal {
    /// The `category/package` whose USE is proposed to change.
    pub cp: String,
    /// The candidate's enabled USE after applying the toggles.
    pub new_use: BTreeSet<String>,
    /// The per-flag toggles, for reporting as a `package.use` change.
    pub changes: Vec<UseChange>,
}

/// The encoder over a single package's metadata and resolved USE.
pub(crate) struct Encoder<'a, S: ResolveSource> {
    pub source: &'a S,
    /// Branch-leader keys masked by the resolve layer's `||` fallback loop. A
    /// branch whose leader keys are all masked is skipped when choosing.
    pub branch_mask: &'a BTreeSet<String>,
    /// Collector for the `||` branch decisions made during this encoding pass.
    pub branch_points: &'a std::cell::RefCell<Vec<BranchPoint>>,
    /// The `(cp, slot)` keys already pulled into the partial solution, seeded
    /// from the previous solve's decisions and grown as forced alternatives are
    /// chosen during this pass. A slotless atom prefers a key already present
    /// here, mirroring Portage's `preferred_in_graph` bin so a second consumer
    /// reuses an existing slot rather than installing a redundant one.
    pub in_graph: &'a std::cell::RefCell<BTreeSet<String>>,
    /// USE-autounmask overrides seeded by the resolve layer: `cp` to its
    /// proposed enabled USE.
    pub use_overrides: &'a BTreeMap<String, BTreeSet<String>>,
    /// Collector for the USE-autounmask proposals made during this encoding pass.
    pub use_proposals: &'a std::cell::RefCell<Vec<UseProposal>>,
}

impl<'a, S: ResolveSource> Encoder<'a, S> {
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
                    Some(alts) => self.push_disjunction(&mut clauses, alts),
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
                self.push_disjunction(&mut clauses, alts);
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
                self.push_disjunction(clauses, provider_alts);
                return Ok(());
            }
            return Err(format!("no provider for {}", atom.cp));
        }
        self.push_disjunction(clauses, alts);
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
    /// in both `a` and `b`), and its blockers are asserted. A nested `||` group
    /// inside the chosen branch is preserved as its own disjunction rather than
    /// flattened, so `|| ( ( a || ( b c ) ) d )` requires `a` together with one
    /// of `b` or `c`. An any-of over every satisfiable branch's leading atom is
    /// also emitted so the solver can fall back to another branch on a learned
    /// conflict.
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
            for atom in &branch.atoms {
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
            // A nested any-of group with no satisfiable branch makes this branch
            // unsatisfiable; a branch carrying a nested group always requires an
            // install, so the free-branch shortcut below does not apply to it.
            if satisfiable && !branch.nested.is_empty() {
                any_required = true;
                if !branch
                    .nested
                    .iter()
                    .all(|g| self.group_satisfiable(g, parent_use, features))
                {
                    satisfiable = false;
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
                .filter_map(|(idx, _, _)| Self::branch_lead_cp(&group[*idx].atoms));
            match lead.next() {
                Some(first) => lead.all(|cp| cp == first),
                None => false,
            }
        };
        let best: std::collections::HashMap<usize, Option<Version>> = if same_cp {
            branches
                .iter()
                .map(|(idx, _, _)| (*idx, self.branch_best_version(&group[*idx].atoms)))
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

        // A branch's leader keys are the solver keys of its first required atom's
        // alternatives. Masking those keys is how the resolve layer drops a branch.
        let leader_keys = |atoms: &[Vec<Alt>]| -> BTreeSet<String> {
            atoms
                .first()
                .map(|alts| alts.iter().map(|(k, _)| k.clone()).collect())
                .unwrap_or_default()
        };

        // Drop branches the resolve layer has masked after a downstream conflict.
        // A branch is masked once all of its leader keys are masked.
        branches.retain(|(_, atoms, _)| {
            let keys = leader_keys(atoms);
            keys.is_empty() || keys.iter().any(|k| !self.branch_mask.contains(k))
        });
        if branches.is_empty() {
            return Err("every any-of branch is masked by conflict fallback".to_owned());
        }

        // The chosen branch is the first after ordering and masking: emit its full
        // conjunction (every atom required) and assert its blockers.
        let (chosen_idx, chosen_atoms, chosen_blockers) = &branches[0];
        let chosen_idx = *chosen_idx;
        let chosen_leader_keys = leader_keys(chosen_atoms);
        // Record the decision so the resolve layer can mask this branch and force
        // the next one if the chosen branch conflicts downstream.
        if !chosen_leader_keys.is_empty() {
            self.branch_points.borrow_mut().push(BranchPoint {
                chosen_leader_keys,
                has_fallback: branches.len() > 1,
            });
        }
        for alts in chosen_atoms {
            self.push_disjunction(clauses, alts.clone());
        }
        for blocker in chosen_blockers {
            for alt in self.blocked_alternatives(blocker, Some(parent), parent_use, features) {
                conflicts.push(alt);
            }
        }
        // Encode the chosen branch's nested any-of groups, each as its own
        // disjunction, so an inner `||` is preserved rather than flattened.
        for nested in &group[chosen_idx].nested {
            self.encode_group(nested, parent_use, parent, clauses, conflicts, features)?;
        }
        Ok(())
    }

    /// Whether an any-of group has at least one satisfiable branch: a branch
    /// whose required atoms all have alternatives and whose nested groups are
    /// each satisfiable.
    fn group_satisfiable(
        &self,
        group: &Group,
        parent_use: &BTreeSet<String>,
        features: EapiFeatures,
    ) -> bool {
        group
            .iter()
            .any(|branch| self.branch_satisfiable(branch, parent_use, features))
    }

    /// Whether a single any-of branch can be satisfied: every required atom has
    /// at least one alternative and every nested group is satisfiable.
    fn branch_satisfiable(
        &self,
        branch: &Branch,
        parent_use: &BTreeSet<String>,
        features: EapiFeatures,
    ) -> bool {
        for atom in &branch.atoms {
            if atom.blocker != BlockerKind::None || self.atom_is_provided(atom) {
                continue;
            }
            let available = if atom.cp.starts_with("virtual/") {
                self.expand_virtual(atom, features).is_some()
            } else {
                !self
                    .required_alternatives(atom, parent_use, features)
                    .is_empty()
            };
            if !available {
                return false;
            }
        }
        branch
            .nested
            .iter()
            .all(|g| self.group_satisfiable(g, parent_use, features))
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
        let installed = self.source.installed(&atom.cp);
        let mut by_slot: std::collections::BTreeMap<String, Vec<Version>> =
            std::collections::BTreeMap::new();
        for m in self.source.versions_of(&atom.cp) {
            if !version_satisfies(atom, &m.version) || !slot_matches(atom, &m) {
                continue;
            }
            // The installed version of a slot is always admitted as an
            // alternative even when masked, so an already-satisfied dependency is
            // not forced to change or downgraded (Portage's `_iter_match_pkgs_any`
            // / `_downgrade_probe`); other masked versions stay excluded.
            let installed_here = installed
                .iter()
                .any(|i| i.version == m.version && i.slot == m.slot);
            if !self.source.is_visible(&m) && !installed_here {
                continue;
            }
            let cand_use = self.effective_use(&m);
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
                let cand_use = self.effective_use(&m);
                if !use_deps_satisfied(atom, &cand_use, &m.iuse, parent_use, features) {
                    continue;
                }
                by_slot.entry(m.slot.clone()).or_default().push(m.version);
            }
        }
        if by_slot.is_empty() && !atom.use_deps.is_empty() {
            // USE-dependency autounmask (Portage's level 0): no visible candidate's
            // USE satisfies the atom, so propose toggling only settable flags on the
            // best matching candidate and admit it under the proposed USE.
            let mut matching: Vec<PackageMeta> = self
                .source
                .versions_of(&atom.cp)
                .into_iter()
                .filter(|m| {
                    version_satisfies(atom, &m.version)
                        && slot_matches(atom, m)
                        && self.source.is_visible(m)
                })
                .collect();
            // Highest version first, mirroring the visible candidate order.
            matching.sort_by(|a, b| b.version.cmp(&a.version));
            for m in matching {
                let cand_use = self.effective_use(&m);
                let locked = self.source.locked_use(&m);
                if let Some((new_use, changes)) =
                    self.use_toggles(atom, &cand_use, &m.iuse, &locked, parent_use, features)
                {
                    self.use_proposals.borrow_mut().push(UseProposal {
                        cp: atom.cp.clone(),
                        new_use,
                        changes,
                    });
                    by_slot.entry(m.slot.clone()).or_default().push(m.version);
                    break;
                }
            }
        }
        self.to_alternatives(&atom.cp, by_slot)
    }

    /// The effective enabled USE for a candidate: the seeded USE-autounmask
    /// override for its `cp` when present, otherwise the source's resolved USE.
    fn effective_use(&self, meta: &PackageMeta) -> BTreeSet<String> {
        self.use_overrides
            .get(&meta.cp)
            .cloned()
            .unwrap_or_else(|| self.source.resolved_use(meta))
    }

    /// Derive the USE toggles that make a candidate satisfy an atom's
    /// USE-dependency, considering only settable flags (declared in the
    /// candidate's IUSE and not pinned by `use.mask`/`use.force`). Returns the
    /// candidate's enabled USE after the toggles and the per-flag changes, or
    /// `None` when no settable toggle makes the candidate satisfy the atom, so a
    /// candidate whose dependency cannot be met stays unsatisfiable.
    fn use_toggles(
        &self,
        atom: &NormAtom,
        cand_use: &BTreeSet<String>,
        cand_iuse: &BTreeSet<String>,
        locked: &BTreeSet<String>,
        parent_use: &BTreeSet<String>,
        features: EapiFeatures,
    ) -> Option<(BTreeSet<String>, Vec<UseChange>)> {
        let mut new_use = cand_use.clone();
        let mut changes: Vec<UseChange> = Vec::new();
        for dep in &atom.use_deps {
            // Only a settable flag can be toggled; a locked or undeclared flag is
            // left as is and verified against the dependency below.
            if !cand_iuse.contains(&dep.flag) || locked.contains(&dep.flag) {
                continue;
            }
            let parent_enabled = parent_use.contains(&dep.flag);
            let need = match dep.kind {
                UseReqKind::Enabled => Some(true),
                UseReqKind::Disabled => Some(false),
                UseReqKind::EnabledIfParent => parent_enabled.then_some(true),
                UseReqKind::DisabledIfParent => (!parent_enabled).then_some(false),
                UseReqKind::EqualToParent => Some(parent_enabled),
                UseReqKind::OppositeToParent => Some(!parent_enabled),
            };
            let Some(need) = need else { continue };
            if new_use.contains(&dep.flag) == need {
                continue;
            }
            if need {
                new_use.insert(dep.flag.clone());
            } else {
                new_use.remove(&dep.flag);
            }
            changes.push(UseChange {
                flag: dep.flag.clone(),
                enable: need,
            });
        }
        // No toggle helps, or the toggled USE still does not satisfy every
        // dependency (an undeclared or locked flag the atom requires): leave the
        // candidate unsatisfiable.
        if changes.is_empty()
            || !use_deps_satisfied(atom, &new_use, cand_iuse, parent_use, features)
        {
            return None;
        }
        Some((new_use, changes))
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
            // Every provider, including those inside a nested `||`, becomes a
            // provider alternative, preserving the inner disjunction rather than
            // dropping it.
            for g in &groups {
                flatten_group_atoms(g, &mut collected);
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
            // Only the highest visible matching virtual version endorses
            // providers; lower versions are not offered as fallbacks, keeping the
            // offered set consistent with the post-solve `emit_virtual_edges`,
            // which records an edge only for the highest matching virtual.
            break;
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

    /// Push a disjunction over alternatives, forcing the most-preferred as a
    /// positive requirement so the solver selects it, with the disjunction over
    /// the non-masked alternatives kept as a fallback for conflict-driven
    /// switching.
    ///
    /// The generic solver only decides packages that are positively required, so
    /// a bare disjunction (whose alternatives appear only as negative terms)
    /// would never drive a selection. Following `dep_zapdeps`, the encoder forces
    /// one alternative using its preference bins: an alternative whose
    /// `(cp, slot)` is already in the partial graph (preferred-in-graph) is
    /// chosen first, then one whose `(cp, slot)` is installed
    /// (preferred-installed), then the leading alternative, which
    /// `to_alternatives` has already ordered highest-version-first (upgrade
    /// promotion).
    ///
    /// An alternative the resolve layer has masked after a downstream conflict is
    /// dropped from the choice, and a multi-alternative disjunction records a
    /// [`BranchPoint`] so the resolve layer can mask the forced slot and fall
    /// back to a lower one, exactly as it relaxes a `||` branch.
    fn push_disjunction(&self, clauses: &mut Vec<Clause<String, Version>>, alternatives: Vec<Alt>) {
        if alternatives.is_empty() {
            return;
        }
        // Drop alternatives whose `(cp, slot)` the resolve layer has masked, so a
        // re-solve forces a different slot. If every alternative is masked, fall
        // back to all of them so the request still drives a selection.
        let live: Vec<usize> = (0..alternatives.len())
            .filter(|&i| !self.branch_mask.contains(&alternatives[i].0))
            .collect();
        let pool: Vec<usize> = if live.is_empty() {
            (0..alternatives.len()).collect()
        } else {
            live
        };

        let in_graph = self.in_graph.borrow();
        let pick = pool
            .iter()
            .copied()
            .find(|&i| in_graph.contains(&alternatives[i].0))
            .or_else(|| {
                pool.iter().copied().find(|&i| {
                    crate::provider::split_key(&alternatives[i].0)
                        .map(|(cp, slot)| self.source.installed(cp).iter().any(|x| x.slot == slot))
                        .unwrap_or(false)
                })
            })
            .or_else(|| pool.first().copied());
        drop(in_graph);

        if let Some(i) = pick {
            let (key, term) = &alternatives[i];
            clauses.push(Clause::single(key.clone(), term.clone()));
            // Record the forced slot in the graph so a later consumer of the same
            // slotless atom reuses it rather than adding a redundant slot.
            self.in_graph.borrow_mut().insert(key.clone());
            // A multi-slot atom's forced slot is relaxable: record it so the
            // resolve layer can mask it and re-solve onto a lower slot.
            if alternatives.len() > 1 {
                self.branch_points.borrow_mut().push(BranchPoint {
                    chosen_leader_keys: std::iter::once(key.clone()).collect(),
                    has_fallback: pool.len() > 1,
                });
            }
        }
        if pool.len() > 1 {
            let alts: Vec<Alt> = pool.iter().map(|&i| alternatives[i].clone()).collect();
            clauses.push(Clause::any_of(alts));
        }
    }
}

/// Collect every atom of an any-of group, descending through nested groups, into
/// a flat alternative list. Virtual provider expansion treats a nested `||`
/// group as additional provider alternatives, so the inner disjunction is
/// preserved rather than dropped.
fn flatten_group_atoms<'b>(group: &Group<'b>, out: &mut Vec<&'b NormAtom>) {
    for branch in group {
        out.extend(branch.atoms.iter().copied());
        for nested in &branch.nested {
            flatten_group_atoms(nested, out);
        }
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

    fn glob_atom(prefix: &str) -> NormAtom {
        NormAtom {
            blocker: crate::depnode::BlockerKind::None,
            cp: "dev-lang/python".to_owned(),
            version: Some((Op::EqualGlob, Version::parse(prefix).unwrap())),
            slot: None,
            subslot: None,
            slot_op: None,
            use_deps: vec![],
        }
    }

    #[test]
    fn equal_glob_matches_on_component_boundaries() {
        // The resolver path must agree with the atom path: `=...*` stops at a
        // version-component boundary, so a longer numeric component is not a match.
        let one_two = glob_atom("1.2");
        assert!(version_satisfies(&one_two, &Version::parse("1.2").unwrap()));
        assert!(version_satisfies(
            &one_two,
            &Version::parse("1.2.3").unwrap()
        ));
        assert!(!version_satisfies(
            &one_two,
            &Version::parse("1.20").unwrap()
        ));

        let three_one = glob_atom("3.1");
        assert!(version_satisfies(
            &three_one,
            &Version::parse("3.1").unwrap()
        ));
        assert!(!version_satisfies(
            &three_one,
            &Version::parse("3.10").unwrap()
        ));
        assert!(!version_satisfies(
            &three_one,
            &Version::parse("3.11").unwrap()
        ));

        // Non-digit-to-digit boundary and leading-zero normalization.
        let one_alpha = glob_atom("1_alpha");
        assert!(version_satisfies(
            &one_alpha,
            &Version::parse("1_alpha").unwrap()
        ));
        assert!(version_satisfies(
            &one_alpha,
            &Version::parse("1_alpha1").unwrap()
        ));
        let one = glob_atom("1");
        assert!(!version_satisfies(&one, &Version::parse("10").unwrap()));
        assert!(version_satisfies(&one, &Version::parse("1.5").unwrap()));
    }

    #[test]
    fn arch_conditional_reduces_to_edge_on_matching_profile() {
        // The profile arch keyword flows into the resolved USE, so an
        // arch-conditional dependency group is live on the matching profile.
        let dep = NormAtom {
            blocker: crate::depnode::BlockerKind::None,
            cp: "dev-libs/arch-only".to_owned(),
            version: None,
            slot: None,
            subslot: None,
            slot_op: None,
            use_deps: vec![],
        };
        let group = DepNode::Conditional {
            flag: "amd64".to_owned(),
            sense: true,
            body: vec![DepNode::Leaf(dep)],
        };

        // On an amd64 profile the branch contributes its edge.
        let on_amd64: BTreeSet<String> = ["amd64".to_owned()].into_iter().collect();
        let mut atoms = Vec::new();
        let mut groups = Vec::new();
        reduce(&group, &on_amd64, &mut atoms, &mut groups);
        assert!(atoms.iter().any(|a| a.cp == "dev-libs/arch-only"));

        // On a non-amd64 profile the same group contributes no edge.
        let other: BTreeSet<String> = BTreeSet::new();
        let mut atoms = Vec::new();
        let mut groups = Vec::new();
        reduce(&group, &other, &mut atoms, &mut groups);
        assert!(atoms.is_empty());
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
