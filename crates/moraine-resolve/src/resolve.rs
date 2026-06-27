//! The single resolution entry point: request to resolved solution.

use std::collections::{BTreeMap, BTreeSet};

use moraine_atom::Atom;
use moraine_common::Interner;
use moraine_eapi::{PERMISSIVE, features_for};
use moraine_solver::{Explanation, solve_with_stats};
use moraine_version::Version;
use tracing::instrument;

use crate::depnode::{BlockerKind, DepNode, NormAtom, SlotOpKind};
use crate::encode::{CLASSES, root_for, slot_matches, use_deps_satisfied, version_satisfies};
use crate::error::ResolveError;
use crate::normalize::normalize_atom;
use crate::provider::{GentooProvider, REQUEST_CP, package_key, split_key};
use crate::solution::{
    AutounmaskChange, BlockVictim, DepClass, DepEdge, RecordedBlocker, ResolvedPackage,
    ResolvedSolution, SlotBinding,
};
use crate::source::{AcceptChange, Acceptability, PackageMeta, ResolveSource, UseChange};

/// The package manager's own `category/package`, which a blocker may never
/// uninstall.
const PACKAGE_MANAGER: &str = "sys-apps/portage";

/// The autounmask policy: which soft-mask dimensions the resolver may relax on
/// its own, mirroring Portage's `autounmask_keep_*` `myparams` flags. A `keep`
/// flag left on means the resolver reports the change as a suggestion and
/// refuses to apply it, while a `keep` flag turned off means the resolver
/// applies the change and proceeds.
///
/// The [`Default`] matches Portage's defaults (`create_depgraph_params`):
/// keyword and license unmasking are kept locked (`keep_keywords` and
/// `keep_license` on), while USE unmasking is enabled (`keep_use` off).
#[derive(Debug, Clone, Copy)]
pub struct AutounmaskPolicy {
    /// Keep `~arch` keyword masks locked, reporting a keyword change as a
    /// suggestion rather than applying it (Portage `autounmask_keep_keywords`).
    pub keep_keywords: bool,
    /// Keep non-accepted licenses locked, reporting a license change as a
    /// suggestion rather than applying it (Portage `autounmask_keep_license`).
    pub keep_license: bool,
    /// Keep the active USE locked, suppressing USE-dependency autounmask
    /// (Portage `autounmask_keep_use`). Off by default, so a USE change is
    /// proposed and applied.
    pub keep_use: bool,
}

impl Default for AutounmaskPolicy {
    fn default() -> Self {
        AutounmaskPolicy {
            keep_keywords: true,
            keep_license: true,
            keep_use: false,
        }
    }
}

/// Resolution behavior modifiers, mirroring `emerge`'s `--deep`/`--update`/
/// `--newuse`. The default (all `false`) keeps the conservative behavior: an
/// installed version is preferred and only an actually-changed package is
/// rebuilt.
#[derive(Debug, Clone, Copy, Default)]
pub struct Modifiers {
    /// Rank the highest visible version ahead of the installed one (`--update`).
    pub update: bool,
    /// Run the consistency pass across the installed dependency graph, not just
    /// the request (`--deep`).
    pub deep: bool,
    /// The optional `--deep` depth bound. A depth of zero disables the deep
    /// consistency pass even when `deep` is set, matching `emerge`'s `deep != 0`;
    /// `None` and any positive depth run it. `None` means unbounded.
    pub deep_depth: Option<u32>,
    /// Treat a USE-flag change against the installed package as a reinstall
    /// trigger (`--newuse`).
    pub newuse: bool,
    /// Reinstall an installed package whose ebuild dependencies changed,
    /// comparing slot-stripped `*DEPEND` (`--changed-deps`).
    pub changed_deps: bool,
    /// Reinstall an installed package whose ebuild slot or sub-slot changed
    /// (`--changed-slot`).
    pub changed_slot: bool,
    /// The autounmask policy governing whether a keyword, license, or USE change
    /// is applied or only reported.
    pub autounmask: AutounmaskPolicy,
}

/// Resolve a set of request atom strings against the given source, producing a
/// resolved solution or a structured failure.
pub fn resolve<S: ResolveSource>(
    source: &S,
    requests: &[&str],
) -> Result<ResolvedSolution, ResolveError> {
    resolve_with(source, requests, Modifiers::default())
}

/// Resolve with explicit [`Modifiers`]. [`resolve`] is this with the default
/// (conservative) modifiers.
#[instrument(skip(source, modifiers), fields(requests = requests.len()))]
pub fn resolve_with<S: ResolveSource>(
    source: &S,
    requests: &[&str],
    modifiers: Modifiers,
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

    let root_version = Version::parse("0").expect("synthetic version parses");

    // Slot-operator reverse-dependency pull-in: an installed consumer of a
    // provider whose sub-slot changed must be rebuilt, but it is not in the
    // request and so never enters the solution. After each solve, find such
    // consumers, add them to the request, and re-solve, bounded by a restart cap
    // to guarantee termination (Portage's `_slot_operator_update_backtrack` /
    // `_need_restart`).
    const MAX_SLOT_RESTARTS: u32 = 8;
    // Bound on the number of `||` branch-fallback re-solves, so a pathological
    // graph cannot loop forever. Each fallback masks at least one new branch
    // leader, so the number of distinct fallbacks is finite regardless.
    const MAX_BRANCH_FALLBACKS: u32 = 64;
    // Bound on the USE-autounmask re-solves, mirroring the slot-restart cap. Each
    // re-solve seeds at least one new override, so the count is finite.
    const MAX_USE_RESTARTS: u32 = 8;
    let mut requested_cps: BTreeSet<String> = request_atoms.iter().map(|a| a.cp.clone()).collect();
    let mut extra_atoms: Vec<NormAtom> = Vec::new();
    let mut restarts = 0u32;
    // `||` branch-leader keys masked so far, and a count of the fallback re-solves.
    let mut branch_mask: BTreeSet<String> = BTreeSet::new();
    let mut branch_fallbacks = 0u32;
    // USE-autounmask overrides accumulated so far (`cp` to proposed enabled USE)
    // and the per-flag changes to report, plus a count of the re-solves.
    let mut use_overrides: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut use_changes: BTreeMap<String, Vec<UseChange>> = BTreeMap::new();
    let mut use_restarts = 0u32;

    loop {
        let mut all_atoms = request_atoms.clone();
        all_atoms.extend(extra_atoms.iter().cloned());
        let provider = GentooProvider::with_request(
            source,
            all_atoms,
            modifiers,
            branch_mask.clone(),
            use_overrides.clone(),
        );

        let (solution, stats) =
            solve_with_stats(&provider, REQUEST_CP.to_owned(), root_version.clone());
        let decisions = match solution {
            Ok(map) => map,
            Err(failure) => {
                // A `||` group's forced branch may have created the conflict. If a
                // recorded branch decision is implicated in the failure and has an
                // unmasked fallback branch, mask its leader and re-solve, mirroring
                // Portage's `dep_zapdeps` + backtracking switching branches.
                if branch_fallbacks < MAX_BRANCH_FALLBACKS {
                    let mut conflict_keys = BTreeSet::new();
                    collect_conflict_keys(&failure.explanation, &mut conflict_keys);
                    let next = provider.branch_points().into_iter().find(|bp| {
                        bp.has_fallback
                            && bp
                                .chosen_leader_keys
                                .iter()
                                .any(|k| conflict_keys.contains(k))
                            && bp
                                .chosen_leader_keys
                                .iter()
                                .any(|k| !branch_mask.contains(k))
                    });
                    if let Some(bp) = next {
                        branch_mask.extend(bp.chosen_leader_keys);
                        branch_fallbacks += 1;
                        continue;
                    }
                }
                return Err(ResolveError::Unsatisfiable {
                    explanation: render_explanation(&failure.explanation),
                });
            }
        };

        // USE-autounmask: fold any newly proposed USE override into the
        // accumulated set and re-solve, so the candidate's own dependencies are
        // reduced against the proposed USE. Bounded like the slot-restart loop.
        if use_restarts < MAX_USE_RESTARTS {
            let mut new_override = false;
            for proposal in provider.use_proposals() {
                let changed = use_overrides
                    .get(&proposal.cp)
                    .is_none_or(|existing| *existing != proposal.new_use);
                if changed {
                    use_overrides.insert(proposal.cp.clone(), proposal.new_use);
                    use_changes.insert(proposal.cp, proposal.changes);
                    new_override = true;
                }
            }
            if new_override {
                use_restarts += 1;
                continue;
            }
        }

        let mut resolved =
            assemble_solution(source, &decisions, modifiers, &use_overrides, &use_changes)?;
        // Fold the `||` branch-fallback, slot-restart, and USE-autounmask
        // re-solves into the reported count, distinct from the solver's inner
        // backjumps (`stats`).
        resolved.backtracks = stats.backtracks + restarts + branch_fallbacks + use_restarts;

        if restarts < MAX_SLOT_RESTARTS {
            let pulled = rebuild_consumers(source, &resolved, &requested_cps);
            if !pulled.is_empty() {
                for atom in pulled {
                    requested_cps.insert(atom.cp.clone());
                    extra_atoms.push(atom);
                }
                restarts += 1;
                continue;
            }
        }
        return Ok(resolved);
    }
}

/// Find installed reverse-dependencies that must be rebuilt because a selected
/// provider's sub-slot differs from their recorded `:=` binding, returned as bare
/// request atoms to pull into the next re-solve. Consumers already in the request
/// or already selected are skipped, bounding the pull-in.
fn rebuild_consumers<S: ResolveSource>(
    source: &S,
    resolved: &ResolvedSolution,
    requested: &BTreeSet<String>,
) -> Vec<NormAtom> {
    // Index the selected providers by cp once, so the installed scan is a single
    // pass rather than a re-scan of the whole installed store per provider.
    let mut providers: std::collections::HashMap<&str, Vec<&ResolvedPackage>> =
        std::collections::HashMap::new();
    let mut selected_cps: BTreeSet<&str> = BTreeSet::new();
    for p in &resolved.packages {
        providers.entry(p.cp.as_str()).or_default().push(p);
        selected_cps.insert(p.cp.as_str());
    }

    let mut out: Vec<NormAtom> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for inst in source.installed_all() {
        if requested.contains(&inst.cp)
            || seen.contains(&inst.cp)
            || selected_cps.contains(inst.cp.as_str())
        {
            continue;
        }
        let needs_rebuild = inst.slot_bindings.iter().any(|(dep_cp, bslot, bsub)| {
            if bslot.is_empty() {
                return false;
            }
            // PMS 7.2: a missing sub-slot equals the slot.
            let bound_sub = bsub.as_deref().unwrap_or(bslot.as_str());
            providers.get(dep_cp.as_str()).is_some_and(|ps| {
                ps.iter().any(|p| {
                    &p.slot == bslot && p.subslot.as_deref().unwrap_or(p.slot.as_str()) != bound_sub
                })
            })
        });
        if needs_rebuild {
            seen.insert(inst.cp.clone());
            out.push(bare_atom(&inst.cp));
        }
    }
    out
}

/// A bare request atom for `cp` with no version, slot, or USE constraint, used to
/// pull a reverse-dependency into resolution.
fn bare_atom(cp: &str) -> NormAtom {
    NormAtom {
        blocker: BlockerKind::None,
        cp: cp.to_owned(),
        version: None,
        slot: None,
        subslot: None,
        slot_op: None,
        use_deps: Vec::new(),
    }
}

/// Build the resolved solution from the solver's `cp:slot -> version` decisions.
fn assemble_solution<S: ResolveSource>(
    source: &S,
    decisions: &BTreeMap<String, Version>,
    modifiers: Modifiers,
    use_overrides: &BTreeMap<String, BTreeSet<String>>,
    use_changes: &BTreeMap<String, Vec<UseChange>>,
) -> Result<ResolvedSolution, ResolveError> {
    // The selected packages, grouped by cp so two slots of one cp both appear.
    let mut selected: BTreeMap<String, Vec<(Version, PackageMeta)>> = BTreeMap::new();
    for (key, version) in decisions {
        let Some((cp, slot)) = split_key(key) else {
            continue;
        };
        if let Some(meta) = source
            .versions_of(cp)
            .into_iter()
            .find(|m| m.slot == slot && &m.version == version)
        {
            selected
                .entry(cp.to_owned())
                .or_default()
                .push((version.clone(), meta));
        }
    }

    let mut packages: Vec<ResolvedPackage> = Vec::new();
    let mut edges: Vec<DepEdge> = Vec::new();
    let mut blockers: Vec<RecordedBlocker> = Vec::new();
    let mut autounmask: Vec<AutounmaskChange> = Vec::new();

    for (cp, slots) in &selected {
        for (version, meta) in slots {
            // The selected package's USE reflects any USE-autounmask override, so
            // its recorded USE and conditional edges match the proposed change.
            let resolved_use = use_overrides
                .get(cp)
                .cloned()
                .unwrap_or_else(|| source.resolved_use(meta));
            let features = features_for(&meta.eapi);
            // `--newuse` turns a USE change against the installed package into a
            // reinstall, so an unchanged-version install with a different USE set
            // is no longer treated as already installed.
            let already_installed = source.installed_matches(cp, version, &meta.slot)
                && !(modifiers.newuse && use_changed(source, cp, &meta.slot, &resolved_use));

            // Autounmask: a newly-merged package the solver could only reach via a
            // soft mask records the keyword/license change the user must accept.
            // The change is auto-applied only when the policy unlocks its
            // dimension, otherwise it is reported as a suggestion and the install
            // path refuses it.
            if !already_installed
                && let Acceptability::NeedsAccept(change) = source.acceptability(meta)
            {
                let auto_applied = change_auto_applied(&change, &modifiers.autounmask);
                autounmask.push(AutounmaskChange {
                    cp: cp.clone(),
                    version: version.clone(),
                    change,
                    auto_applied,
                });
            }

            // USE-dependency autounmask: a proposed `package.use` change, applied
            // by default (`autounmask_keep_use` off) so resolution proceeded.
            if let Some(changes) = use_changes.get(cp).filter(|c| !c.is_empty()) {
                let change = AcceptChange {
                    use_changes: changes.clone(),
                    ..Default::default()
                };
                let auto_applied = change_auto_applied(&change, &modifiers.autounmask);
                autounmask.push(AutounmaskChange {
                    cp: cp.clone(),
                    version: version.clone(),
                    change,
                    auto_applied,
                });
            }

            // Slot-operator rebuild detection: if this package is installed and
            // recorded a `:=` binding whose provider is now selected with a
            // different sub-slot, the existing build is stale and must be rebuilt.
            let mut subslot_rebuild = false;
            for inst in source.installed(cp) {
                for (dep_cp, bslot, bsub) in &inst.slot_bindings {
                    // A legacy record written before `:=` bindings were baked
                    // carries an empty slot (unbound). Treat it as not-a-binding
                    // rather than comparing against an empty slot, which would
                    // mis-fire; such packages fall back to `--changed-slot`/
                    // `@preserved-rebuild`.
                    if bslot.is_empty() {
                        continue;
                    }
                    // PMS 7.2: a missing sub-slot equals the slot. The recorded
                    // binding is rewritten to `slot/subslot=` at build time, so an
                    // unspecified store sub-slot must default to the slot before
                    // comparing, or every bare-slot provider looks rebuilt.
                    let bound_sub = bsub.as_deref().unwrap_or(bslot.as_str());
                    if let Some(dep_slots) = selected.get(dep_cp)
                        && dep_slots.iter().any(|(_, dm)| {
                            &dm.slot == bslot
                                && dm.subslot.as_deref().unwrap_or(dm.slot.as_str()) != bound_sub
                        })
                    {
                        subslot_rebuild = true;
                    }
                }
            }

            // `--changed-slot`: the current ebuild declares a slot or sub-slot
            // different from the installed package's recorded one (same version).
            if modifiers.changed_slot
                && source.installed(cp).iter().any(|inst| {
                    &inst.version == version
                        && (inst.slot != meta.slot || inst.subslot != meta.subslot)
                })
            {
                subslot_rebuild = true;
            }

            // `--changed-deps`: the current ebuild's slot-stripped dependencies
            // differ from those recorded for the installed package.
            if modifiers.changed_deps
                && source.installed(cp).iter().any(|inst| {
                    &inst.version == version
                        && !inst.recorded_deps.is_empty()
                        && deps_changed(meta, inst)
                })
            {
                subslot_rebuild = true;
            }

            let mut slot_bindings: Vec<SlotBinding> = Vec::new();

            let class_nodes: [(&DepNode, DepClass); 5] = [
                (&meta.bdepend, DepClass::Bdepend),
                (&meta.depend, DepClass::Depend),
                (&meta.rdepend, DepClass::Rdepend),
                (&meta.pdepend, DepClass::Pdepend),
                (&meta.idepend, DepClass::Idepend),
            ];

            // The edge's source is this specific `(cp, slot)`, so two slots of one
            // cp record their own outgoing edges rather than collapsing.
            let from_key = package_key(cp, &meta.slot);
            for (node, class) in class_nodes {
                let mut atoms: Vec<(&NormAtom, bool)> = Vec::new();
                collect_atoms(node, &resolved_use, false, &mut atoms);
                for (atom, optional) in atoms {
                    emit_edge_for_atom(
                        source,
                        &from_key,
                        atom,
                        class,
                        optional,
                        features,
                        &resolved_use,
                        &selected,
                        &mut edges,
                        &mut blockers,
                    );
                }
                // Record `:=` bindings only for the satisfied `||` branch.
                let root = root_for(class, features);
                record_slot_bindings(node, &resolved_use, &selected, root, &mut slot_bindings);
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
    }

    // Safety: a blocker is never allowed to remove the package manager itself.
    for blocker in &blockers {
        if let Some(victim) = blocker.victims.iter().find(|v| v.cp == PACKAGE_MANAGER) {
            return Err(ResolveError::UnresolvableBlocker {
                blocker: blocker.blocker.clone(),
                victim: victim.cp.clone(),
                reason: "the package manager cannot be uninstalled".to_owned(),
            });
        }
    }

    // Enforce blockers declared by packages that stay installed against the
    // newly-merged set: an installed `Y` declaring `!cat/x` blocks installing
    // cat/x when Y is not itself being removed or replaced.
    enforce_installed_blockers(source, &packages)?;

    // `--deep`: validate that no changed package leaves an installed
    // reverse-dependency's atom unsatisfied (Portage's `_complete_graph`,
    // gated on a version actually changing). A `--deep=0` disables the pass,
    // matching Portage's `deep != 0`.
    if modifiers.deep && modifiers.deep_depth != Some(0) {
        enforce_reverse_dep_consistency(source, &packages)?;
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

    autounmask.sort_by(|a, b| a.cp.cmp(&b.cp).then_with(|| a.version.cmp(&b.version)));

    Ok(ResolvedSolution {
        packages,
        edges,
        blockers,
        backtracks: 0,
        autounmask,
    })
}

/// Whether an autounmask change is applied by the resolver under the policy: a
/// change dimension is applied only when its matching keep flag is off,
/// mirroring `_autounmask_levels` yielding a level only when the keep flag is
/// false. A change carrying a kept-locked dimension is a suggestion, not applied.
fn change_auto_applied(change: &AcceptChange, policy: &AutounmaskPolicy) -> bool {
    (change.keyword.is_none() || !policy.keep_keywords)
        && (change.licenses.is_empty() || !policy.keep_license)
        && (change.use_changes.is_empty() || !policy.keep_use)
}

/// Whether the resolved USE for a `(cp, slot)` differs from the installed
/// package's recorded enabled USE, the `--newuse` reinstall trigger.
fn use_changed<S: ResolveSource>(
    source: &S,
    cp: &str,
    slot: &str,
    resolved_use: &BTreeSet<String>,
) -> bool {
    source
        .installed(cp)
        .into_iter()
        .find(|i| i.slot == slot)
        .is_some_and(|inst| &inst.use_enabled != resolved_use)
}

/// Enforce blockers declared by packages that remain installed against the
/// newly-merged set.
///
/// For each installed package not being replaced at its slot, its RDEPEND and
/// PDEPEND blocker atoms are matched against the packages being newly installed.
/// A match (a newly-merged package the installed package blocks, where the
/// installed package is not itself being removed) is an unresolvable blocker,
/// mirroring Portage reading installed packages' blocker atoms in
/// `_validate_blockers`.
fn enforce_installed_blockers<S: ResolveSource>(
    source: &S,
    packages: &[ResolvedPackage],
) -> Result<(), ResolveError> {
    for inst in source.installed_all() {
        // A package being replaced at its own slot no longer governs: its
        // replacement's blockers are emitted through the normal encoding path.
        if packages
            .iter()
            .any(|p| p.cp == inst.cp && p.slot == inst.slot)
        {
            continue;
        }
        // The installed package's blocker atoms come from its repository ebuild,
        // reduced against its recorded USE. Without a repository entry its deps
        // are unknown, so it contributes no enforceable blocker.
        let Some(imeta) = source
            .versions_of(&inst.cp)
            .into_iter()
            .find(|m| m.version == inst.version && m.slot == inst.slot)
        else {
            continue;
        };
        let features = features_for(&imeta.eapi);
        let mut atoms: Vec<(&NormAtom, bool)> = Vec::new();
        collect_atoms(&imeta.rdepend, &inst.use_enabled, false, &mut atoms);
        collect_atoms(&imeta.pdepend, &inst.use_enabled, false, &mut atoms);
        for (atom, _) in atoms {
            if atom.blocker == BlockerKind::None {
                continue;
            }
            for p in packages {
                // Only a genuinely new merge can violate the block; an unchanged
                // already-installed package coexisted before this run.
                if p.already_installed || p.cp != atom.cp {
                    continue;
                }
                let slot_ok = atom.slot.as_ref().is_none_or(|s| &p.slot == s);
                if slot_ok
                    && version_satisfies(atom, &p.version)
                    && use_deps_satisfied(
                        atom,
                        &p.use_enabled,
                        &p.use_enabled,
                        &inst.use_enabled,
                        features,
                    )
                {
                    return Err(ResolveError::UnresolvableBlocker {
                        blocker: inst.cp.clone(),
                        victim: p.cp.clone(),
                        reason: "an installed package blocks it and is not being removed"
                            .to_owned(),
                    });
                }
            }
        }
    }
    Ok(())
}

/// The `--deep` reverse-dependency consistency pass.
///
/// For each installed package not itself being changed, its runtime
/// (RDEPEND/PDEPEND) atoms on a changed package are re-checked against the
/// post-resolution providers. If a changed package no longer satisfies an
/// installed consumer's atom and nothing else provides it, the change would
/// break the consumer, surfaced as a structured
/// [`ResolveError::BrokenReverseDependency`] rather than a silent breakage.
fn enforce_reverse_dep_consistency<S: ResolveSource>(
    source: &S,
    packages: &[ResolvedPackage],
) -> Result<(), ResolveError> {
    // Packages that actually changed (a new install or a version change), the
    // only ones whose reverse-dependencies need re-validation.
    let changed: BTreeSet<&str> = packages
        .iter()
        .filter(|p| !p.already_installed)
        .map(|p| p.cp.as_str())
        .collect();
    if changed.is_empty() {
        return Ok(());
    }

    for inst in source.installed_all() {
        // A consumer being changed itself is rebuilt against the new providers,
        // so its old atoms do not constrain the result.
        if packages
            .iter()
            .any(|p| p.cp == inst.cp && p.slot == inst.slot && !p.already_installed)
        {
            continue;
        }
        let Some(imeta) = source
            .versions_of(&inst.cp)
            .into_iter()
            .find(|m| m.version == inst.version && m.slot == inst.slot)
        else {
            continue;
        };
        let features = features_for(&imeta.eapi);
        let mut atoms: Vec<(&NormAtom, bool)> = Vec::new();
        collect_atoms(&imeta.rdepend, &inst.use_enabled, false, &mut atoms);
        collect_atoms(&imeta.pdepend, &inst.use_enabled, false, &mut atoms);
        for (atom, _) in atoms {
            if atom.blocker != BlockerKind::None || !changed.contains(atom.cp.as_str()) {
                continue;
            }
            if !atom_satisfied_post_solve(source, atom, packages, features) {
                return Err(ResolveError::BrokenReverseDependency {
                    dependent: inst.cp.clone(),
                    dependency: atom.cp.clone(),
                    atom: render_atom(atom),
                });
            }
        }
    }
    Ok(())
}

/// Whether `atom` is satisfied by the post-resolution world: any selected
/// package, or any installed package of the atom's cp whose slot the solution
/// did not change.
fn atom_satisfied_post_solve<S: ResolveSource>(
    source: &S,
    atom: &NormAtom,
    packages: &[ResolvedPackage],
    features: moraine_eapi::EapiFeatures,
) -> bool {
    let selected_slots: BTreeSet<&str> = packages
        .iter()
        .filter(|p| p.cp == atom.cp)
        .map(|p| p.slot.as_str())
        .collect();
    // Selected providers.
    for p in packages.iter().filter(|p| p.cp == atom.cp) {
        let slot_ok = atom.slot.as_ref().is_none_or(|s| &p.slot == s);
        if slot_ok
            && version_satisfies(atom, &p.version)
            && use_deps_satisfied(
                atom,
                &p.use_enabled,
                &p.use_enabled,
                &p.use_enabled,
                features,
            )
        {
            return true;
        }
    }
    // Installed providers in a slot the solution did not touch.
    for inst in source.installed(&atom.cp) {
        if selected_slots.contains(inst.slot.as_str()) {
            continue;
        }
        let slot_ok = atom.slot.as_ref().is_none_or(|s| &inst.slot == s);
        if slot_ok
            && version_satisfies(atom, &inst.version)
            && use_deps_satisfied(
                atom,
                &inst.use_enabled,
                &inst.iuse,
                &inst.use_enabled,
                features,
            )
        {
            return true;
        }
    }
    false
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

/// Find a selected provider of `cp` that satisfies the atom's version and slot.
fn find_provider<'a>(
    selected: &'a BTreeMap<String, Vec<(Version, PackageMeta)>>,
    atom: &NormAtom,
) -> Option<&'a (Version, PackageMeta)> {
    selected
        .get(&atom.cp)?
        .iter()
        .find(|(v, m)| version_satisfies(atom, v) && slot_matches(atom, m))
}

/// The installed entries an actionable blocker removes: those matching the
/// blocker's version, slot, and USE constraints, excluding any `(cp, slot)` the
/// solution is installing (a same-slot install replaces rather than removes it).
fn blocker_victims<S: ResolveSource>(
    source: &S,
    atom: &NormAtom,
    parent_use: &BTreeSet<String>,
    features: moraine_eapi::EapiFeatures,
    selected: &BTreeMap<String, Vec<(Version, PackageMeta)>>,
) -> Vec<BlockVictim> {
    let mut victims = Vec::new();
    for inst in source.installed(&atom.cp) {
        let slot_ok = atom.slot.as_ref().is_none_or(|s| &inst.slot == s);
        if !slot_ok || !version_satisfies(atom, &inst.version) {
            continue;
        }
        if !use_deps_satisfied(
            atom,
            &inst.use_enabled,
            &inst.use_enabled,
            parent_use,
            features,
        ) {
            continue;
        }
        // A same-slot install replaces the installed package rather than removing
        // it, so it is not an uninstall victim.
        let replaced = selected
            .get(&atom.cp)
            .is_some_and(|s| s.iter().any(|(_, m)| m.slot == inst.slot));
        if replaced {
            continue;
        }
        victims.push(BlockVictim {
            cp: atom.cp.clone(),
            version: inst.version.clone(),
            slot: inst.slot.clone(),
        });
    }
    victims
}

#[allow(clippy::too_many_arguments)]
fn emit_edge_for_atom<S: ResolveSource>(
    source: &S,
    from: &str,
    atom: &NormAtom,
    class: DepClass,
    optional: bool,
    features: moraine_eapi::EapiFeatures,
    parent_use: &BTreeSet<String>,
    selected: &BTreeMap<String, Vec<(Version, PackageMeta)>>,
    edges: &mut Vec<DepEdge>,
    blockers: &mut Vec<RecordedBlocker>,
) {
    let root = root_for(class, features);

    if atom.blocker != BlockerKind::None {
        let victims = blocker_victims(source, atom, parent_use, features, selected);
        // A blocker that matches no installed victim and no selected package is
        // irrelevant and is dropped rather than recorded as a phantom uninstall.
        let matches_selected = find_provider(selected, atom).is_some();
        if victims.is_empty() && !matches_selected {
            return;
        }
        blockers.push(RecordedBlocker {
            // The blocking package is identified by its `cp` for display and the
            // safety checks, not by its slot-qualified edge key.
            blocker: crate::solution::endpoint_cp(from).to_owned(),
            blocked_atom: render_atom(atom),
            strong: atom.blocker == BlockerKind::Strong,
            victims,
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

    // Find the selected provider satisfying this atom, and target its slot.
    if let Some((_, dep_meta)) = find_provider(selected, atom) {
        let slot_op = atom.slot_op.is_some();
        edges.push(DepEdge {
            from: from.to_owned(),
            to: package_key(&atom.cp, &dep_meta.slot),
            class,
            root,
            build_time: class.is_build_time(),
            slot_op,
            optional,
        });
    }
}

/// Record the `:=`/`:slot=` slot bindings of a dependency node against the
/// selected providers, descending into only the `||` branch the solution
/// satisfied so a binding is never recorded for an unlinked branch
/// (`_slot_operator.py:88-95`).
fn record_slot_bindings(
    node: &DepNode,
    parent_use: &BTreeSet<String>,
    selected: &BTreeMap<String, Vec<(Version, PackageMeta)>>,
    root: crate::solution::Root,
    out: &mut Vec<SlotBinding>,
) {
    match node {
        DepNode::Leaf(atom) => {
            if matches!(atom.slot_op, Some(SlotOpKind::Equal))
                && let Some((_, dep_meta)) = find_provider(selected, atom)
            {
                out.push(SlotBinding {
                    dependency: atom.cp.clone(),
                    slot: dep_meta.slot.clone(),
                    subslot: dep_meta.subslot.clone(),
                    root,
                });
            }
        }
        DepNode::AllOf(children) => {
            for c in children {
                record_slot_bindings(c, parent_use, selected, root, out);
            }
        }
        DepNode::Conditional { flag, sense, body } => {
            if parent_use.contains(flag) == *sense {
                for c in body {
                    record_slot_bindings(c, parent_use, selected, root, out);
                }
            }
        }
        DepNode::AnyOf(branches)
        | DepNode::ExactlyOneOf(branches)
        | DepNode::AtMostOneOf(branches) => {
            // Only the first branch the solution satisfied is actually linked
            // against, so only its `:=` atoms are bound.
            if let Some(branch) = branches
                .iter()
                .find(|b| branch_satisfied(b, parent_use, selected))
            {
                record_slot_bindings(branch, parent_use, selected, root, out);
            }
        }
    }
}

/// Whether every required (non-blocker) atom of a `||` branch has a selected
/// provider, marking it the branch the solution linked against.
fn branch_satisfied(
    node: &DepNode,
    parent_use: &BTreeSet<String>,
    selected: &BTreeMap<String, Vec<(Version, PackageMeta)>>,
) -> bool {
    let mut atoms: Vec<(&NormAtom, bool)> = Vec::new();
    collect_atoms(node, parent_use, false, &mut atoms);
    atoms.iter().all(|(atom, _)| {
        atom.blocker != BlockerKind::None || find_provider(selected, atom).is_some()
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_virtual_edges<S: ResolveSource>(
    source: &S,
    from: &str,
    atom: &NormAtom,
    class: DepClass,
    optional: bool,
    features: moraine_eapi::EapiFeatures,
    selected: &BTreeMap<String, Vec<(Version, PackageMeta)>>,
    edges: &mut Vec<DepEdge>,
) {
    let root = root_for(class, features);
    // Edge to the virtual itself if it is selected, targeting its slot.
    if let Some((_, vmeta)) = find_provider(selected, atom) {
        edges.push(DepEdge {
            from: from.to_owned(),
            to: package_key(&atom.cp, &vmeta.slot),
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
            if let Some((_, pmeta)) = find_provider(selected, patom) {
                edges.push(DepEdge {
                    from: from.to_owned(),
                    to: package_key(&patom.cp, &pmeta.slot),
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

/// Whether the current ebuild's slot-stripped dependencies differ from the
/// installed package's recorded ones, the `--changed-deps` trigger.
///
/// Both sides are USE-reduced against the installed USE and rendered without
/// slot/sub-slot, so only a structural dependency change (an atom added,
/// removed, or its version constraint or USE-deps changed) is detected, not a
/// slot-operator binding difference, mirroring Portage's `strip_slots`.
fn deps_changed(meta: &PackageMeta, inst: &crate::source::InstalledMeta) -> bool {
    let interner = Interner::new();
    current_dep_set(meta, &inst.use_enabled) != recorded_dep_set(&inst.recorded_deps, &interner)
}

/// The slot-stripped atom set of the current ebuild's dependencies, USE-reduced
/// against `parent_use`.
fn current_dep_set(meta: &PackageMeta, parent_use: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for node in [
        &meta.bdepend,
        &meta.depend,
        &meta.rdepend,
        &meta.pdepend,
        &meta.idepend,
    ] {
        let mut atoms: Vec<(&NormAtom, bool)> = Vec::new();
        collect_atoms(node, parent_use, false, &mut atoms);
        for (atom, _) in atoms {
            out.insert(canonical_atom(atom));
        }
    }
    out
}

/// The slot-stripped atom set parsed from recorded `*DEPEND` strings (already
/// USE-reduced when recorded).
fn recorded_dep_set(recorded: &BTreeMap<String, String>, interner: &Interner) -> BTreeSet<String> {
    let empty = BTreeSet::new();
    let mut out = BTreeSet::new();
    for raw in recorded.values() {
        let Ok(spec) = moraine_atom::DepSpec::parse(raw, PERMISSIVE, interner) else {
            continue;
        };
        let node = crate::normalize::normalize_depspec(&spec, interner);
        let mut atoms: Vec<(&NormAtom, bool)> = Vec::new();
        collect_atoms(&node, &empty, false, &mut atoms);
        for (atom, _) in atoms {
            out.insert(canonical_atom(atom));
        }
    }
    out
}

/// Render an atom without its slot, sub-slot, or slot operator, for a
/// structural dependency comparison. USE-deps are reduced to a sorted flag list.
fn canonical_atom(atom: &NormAtom) -> String {
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
    if !atom.use_deps.is_empty() {
        let mut flags: Vec<&str> = atom.use_deps.iter().map(|u| u.flag.as_str()).collect();
        flags.sort_unstable();
        s.push('[');
        s.push_str(&flags.join(","));
        s.push(']');
    }
    s
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

/// Collect every solver package key that appears anywhere in a failure
/// explanation tree, used to find which `||` branch decision was implicated.
fn collect_conflict_keys(node: &Explanation<String, Version>, out: &mut BTreeSet<String>) {
    match node {
        Explanation::External { terms, .. } => {
            out.extend(terms.iter().map(|(p, _)| p.clone()));
        }
        Explanation::Derived { terms, causes, .. } => {
            out.extend(terms.iter().map(|(p, _)| p.clone()));
            for cause in causes {
                collect_conflict_keys(cause, out);
            }
        }
        Explanation::Shared(_) => {}
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
