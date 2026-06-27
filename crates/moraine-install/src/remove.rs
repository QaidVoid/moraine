//! Removal planning: depclean orphans and prune.
//!
//! These compute *which* installed packages to remove; the merge engine performs
//! the actual unmerge. The logic works over a plain [`InstalledPackage`] model so
//! it is decoupled from the installed store and tested directly. Reachability for
//! depclean is computed over `category/package` keys from a protected root set
//! (the union of the world and system sets).

use std::collections::{BTreeMap, BTreeSet};

use moraine_version::Version;

/// One installed package as the removal planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPackage {
    /// The `category/package-version`.
    pub cpv: String,
    /// The `category/package`.
    pub cp: String,
    /// The resolved slot.
    pub slot: String,
    /// The parsed version, used for prune ordering.
    pub version: Version,
    /// The `category/package` of each runtime dependency.
    pub deps: Vec<String>,
}

/// A planned set of packages to remove, in a stable order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemovalSet {
    /// The `category/package-version` of each package to unmerge.
    pub cpvs: Vec<String>,
}

impl RemovalSet {
    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.cpvs.is_empty()
    }
}

/// Compute the orphan set: installed packages not reachable, directly or
/// transitively, from the protected root set over the dependency edges.
///
/// `roots` holds the protected `category/package` keys (the union of the world
/// and system sets). A package whose `cp` is reachable is retained; everything
/// else is an orphan. Because orphans are unreachable from every retained
/// package, removing them cannot leave a retained package unsatisfied.
pub fn depclean_orphans(installed: &[InstalledPackage], roots: &BTreeSet<String>) -> RemovalSet {
    let reachable = reachable_cps(installed, roots);
    let mut cpvs: Vec<String> = installed
        .iter()
        .filter(|pkg| !reachable.contains(&pkg.cp))
        .map(|pkg| pkg.cpv.clone())
        .collect();
    cpvs.sort();
    RemovalSet { cpvs }
}

/// Compute the orphan set for a targeted depclean, restricting removal to
/// packages whose `cp` is named in `targets`.
///
/// Every installed package whose `cp` is not named is protected by seeding it
/// into the reachability roots, and the named keys are dropped from `roots`. A
/// package is returned as an orphan only when its `cp` is named and is not
/// reachable from the protected set, so a named package is removed only when no
/// protected package still requires it, and every unmatched package is kept.
/// Mirrors Portage's `protected_set` construction for an args-restricted
/// depclean.
pub fn depclean_targeted(
    installed: &[InstalledPackage],
    roots: &BTreeSet<String>,
    targets: &BTreeSet<String>,
) -> RemovalSet {
    let mut protected: BTreeSet<String> = roots
        .iter()
        .filter(|cp| !targets.contains(*cp))
        .cloned()
        .collect();
    for pkg in installed {
        if !targets.contains(&pkg.cp) {
            protected.insert(pkg.cp.clone());
        }
    }
    let reachable = reachable_cps(installed, &protected);
    let mut cpvs: Vec<String> = installed
        .iter()
        .filter(|pkg| targets.contains(&pkg.cp) && !reachable.contains(&pkg.cp))
        .map(|pkg| pkg.cpv.clone())
        .collect();
    cpvs.sort();
    RemovalSet { cpvs }
}

/// Compute the prune set: for each `category/package` keep the single highest
/// installed version across all slots and remove the lower versions. The highest
/// version of every `category/package` is always retained, so no package is ever
/// fully removed. A lower version still required by a retained package is kept by
/// the caller's [`would_break_retained`] guard.
pub fn prune_superseded(installed: &[InstalledPackage]) -> RemovalSet {
    let mut by_cp: BTreeMap<&str, Vec<&InstalledPackage>> = BTreeMap::new();
    for pkg in installed {
        by_cp.entry(pkg.cp.as_str()).or_default().push(pkg);
    }
    let mut cpvs = Vec::new();
    for group in by_cp.values() {
        let Some(highest) = group.iter().map(|p| &p.version).max() else {
            continue;
        };
        for pkg in group {
            if &pkg.version < highest {
                cpvs.push(pkg.cpv.clone());
            }
        }
    }
    cpvs.sort();
    RemovalSet { cpvs }
}

/// Whether removing every `cp` in `removed` would leave a retained package with
/// an unsatisfied dependency. Used to refuse an unsafe explicit removal.
///
/// A retained package is one not being removed; if any retained package depends
/// on a `cp` that no surviving package provides, removal would break it.
pub fn would_break_retained(installed: &[InstalledPackage], removed: &BTreeSet<String>) -> bool {
    let surviving: BTreeSet<&str> = installed
        .iter()
        .filter(|pkg| !removed.contains(&pkg.cpv))
        .map(|pkg| pkg.cp.as_str())
        .collect();
    installed
        .iter()
        .filter(|pkg| !removed.contains(&pkg.cpv))
        .flat_map(|pkg| pkg.deps.iter())
        .any(|dep| !surviving.contains(dep.as_str()))
}

/// The set of `category/package` keys reachable from `roots` over dependency
/// edges, computed by breadth-first traversal.
fn reachable_cps(installed: &[InstalledPackage], roots: &BTreeSet<String>) -> BTreeSet<String> {
    let mut deps_by_cp: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for pkg in installed {
        let entry = deps_by_cp.entry(pkg.cp.as_str()).or_default();
        for dep in &pkg.deps {
            entry.push(dep.as_str());
        }
    }

    let mut reachable: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = roots.iter().cloned().collect();
    while let Some(cp) = queue.pop() {
        if !reachable.insert(cp.clone()) {
            continue;
        }
        if let Some(deps) = deps_by_cp.get(cp.as_str()) {
            for dep in deps {
                if !reachable.contains(*dep) {
                    queue.push((*dep).to_owned());
                }
            }
        }
    }
    reachable
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(cpv: &str, cp: &str, slot: &str, version: &str, deps: &[&str]) -> InstalledPackage {
        InstalledPackage {
            cpv: cpv.to_owned(),
            cp: cp.to_owned(),
            slot: slot.to_owned(),
            version: Version::parse(version).unwrap(),
            deps: deps.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn roots(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn orphan_unreachable_package_is_removed() {
        let installed = vec![
            pkg("app/top-1", "app/top", "0", "1", &["lib/dep"]),
            pkg("lib/dep-1", "lib/dep", "0", "1", &[]),
            pkg("misc/orphan-1", "misc/orphan", "0", "1", &[]),
        ];
        let set = depclean_orphans(&installed, &roots(&["app/top"]));
        assert_eq!(set.cpvs, vec!["misc/orphan-1".to_owned()]);
    }

    #[test]
    fn still_needed_dependency_is_retained() {
        let installed = vec![
            pkg("app/top-1", "app/top", "0", "1", &["lib/dep"]),
            pkg("lib/dep-1", "lib/dep", "0", "1", &[]),
        ];
        let set = depclean_orphans(&installed, &roots(&["app/top"]));
        assert!(set.is_empty());
    }

    #[test]
    fn transitive_dependency_is_reachable() {
        let installed = vec![
            pkg("app/top-1", "app/top", "0", "1", &["lib/mid"]),
            pkg("lib/mid-1", "lib/mid", "0", "1", &["lib/low"]),
            pkg("lib/low-1", "lib/low", "0", "1", &[]),
        ];
        let set = depclean_orphans(&installed, &roots(&["app/top"]));
        assert!(set.is_empty());
    }

    #[test]
    fn prune_keeps_highest_per_cp_across_slots() {
        let installed = vec![
            pkg("lib/a-1", "lib/a", "0", "1", &[]),
            pkg("lib/a-2", "lib/a", "0", "2", &[]),
            pkg("lib/a-3", "lib/a", "1", "3", &[]),
        ];
        let set = prune_superseded(&installed);
        assert_eq!(set.cpvs, vec!["lib/a-1".to_owned(), "lib/a-2".to_owned()]);
    }

    #[test]
    fn prune_across_slots_keeps_highest_version() {
        let installed = vec![
            pkg(
                "dev-lang/python-3.10",
                "dev-lang/python",
                "3.10",
                "3.10",
                &[],
            ),
            pkg(
                "dev-lang/python-3.11",
                "dev-lang/python",
                "3.11",
                "3.11",
                &[],
            ),
        ];
        let set = prune_superseded(&installed);
        assert_eq!(set.cpvs, vec!["dev-lang/python-3.10".to_owned()]);
    }

    #[test]
    fn build_only_dependency_retained_when_included() {
        // `app/top` build-depends on `dev-build/cmake`. With the build edge in
        // the dependency set, cmake is reachable and kept; without it, cmake is
        // an orphan and removed.
        let with_build = vec![
            pkg("app/top-1", "app/top", "0", "1", &["dev-build/cmake"]),
            pkg("dev-build/cmake-1", "dev-build/cmake", "0", "1", &[]),
        ];
        assert!(depclean_orphans(&with_build, &roots(&["app/top"])).is_empty());

        let without_build = vec![
            pkg("app/top-1", "app/top", "0", "1", &[]),
            pkg("dev-build/cmake-1", "dev-build/cmake", "0", "1", &[]),
        ];
        let set = depclean_orphans(&without_build, &roots(&["app/top"]));
        assert_eq!(set.cpvs, vec!["dev-build/cmake-1".to_owned()]);
    }

    #[test]
    fn targeted_depclean_removes_named_protects_unmatched() {
        let installed = vec![
            pkg("app/named-1", "app/named", "0", "1", &[]),
            pkg("misc/orphan-1", "misc/orphan", "0", "1", &[]),
        ];
        let targets = roots(&["app/named"]);
        let set = depclean_targeted(&installed, &roots(&[]), &targets);
        // The named package is removed; the unmatched orphan is protected.
        assert_eq!(set.cpvs, vec!["app/named-1".to_owned()]);
    }

    #[test]
    fn targeted_depclean_keeps_named_still_required() {
        let installed = vec![
            pkg("app/top-1", "app/top", "0", "1", &["lib/dep"]),
            pkg("lib/dep-1", "lib/dep", "0", "1", &[]),
        ];
        // `lib/dep` is named but `app/top` (unmatched, protected) still requires
        // it, so it is not removed.
        let targets = roots(&["lib/dep"]);
        let set = depclean_targeted(&installed, &roots(&["app/top"]), &targets);
        assert!(set.is_empty());
    }

    #[test]
    fn breaking_removal_is_detected() {
        let installed = vec![
            pkg("app/top-1", "app/top", "0", "1", &["lib/dep"]),
            pkg("lib/dep-1", "lib/dep", "0", "1", &[]),
        ];
        let removed = roots(&["lib/dep-1"]);
        assert!(would_break_retained(&installed, &removed));
    }

    #[test]
    fn safe_removal_is_allowed() {
        let installed = vec![
            pkg("app/top-1", "app/top", "0", "1", &[]),
            pkg("misc/orphan-1", "misc/orphan", "0", "1", &[]),
        ];
        let removed = roots(&["misc/orphan-1"]);
        assert!(!would_break_retained(&installed, &removed));
    }
}
