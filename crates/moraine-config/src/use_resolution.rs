//! Effective USE computation: USE_EXPAND flattening, masking and forcing,
//! `package.use` overrides, and `IUSE_EFFECTIVE` derivation.

use std::collections::BTreeSet;

use moraine_atom::{Atom, PackageRef};

use crate::makeconf::VarMap;
use crate::stacking::stack_layers_signed;

/// The effective USE flags for a package, plus the subset marked hidden.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveUse {
    /// The enabled USE flags.
    pub enabled: BTreeSet<String>,
    /// The flags that are enabled but hidden from display.
    pub hidden: BTreeSet<String>,
    /// The flags whose state is fixed by `use.force`/`use.mask` (the user cannot
    /// change them), shown parenthesized in verbose output.
    pub forced: BTreeSet<String>,
}

fn tokens<'a>(env: &'a VarMap, key: &str) -> Vec<&'a str> {
    env.get(key)
        .into_iter()
        .flat_map(str::split_whitespace)
        .collect()
}

/// Flatten `USE_EXPAND` variables into USE flags. Prefixed variables produce
/// `var_value` (lowercased), `USE_EXPAND_UNPREFIXED` variables produce bare
/// values, and `USE_EXPAND_HIDDEN` variables mark their flags hidden.
pub fn flatten_use_expand(env: &VarMap) -> (Vec<String>, BTreeSet<String>) {
    let unprefixed: BTreeSet<&str> = tokens(env, "USE_EXPAND_UNPREFIXED").into_iter().collect();
    let hidden_vars: BTreeSet<&str> = tokens(env, "USE_EXPAND_HIDDEN").into_iter().collect();

    let mut flags = Vec::new();
    let mut hidden = BTreeSet::new();
    for var in tokens(env, "USE_EXPAND") {
        for value in tokens(env, var) {
            let flag = if unprefixed.contains(var) {
                value.to_owned()
            } else {
                format!("{}_{}", var.to_lowercase(), value)
            };
            if hidden_vars.contains(var) {
                hidden.insert(flag.clone());
            }
            flags.push(flag);
        }
    }
    (flags, hidden)
}

/// A `USE_EXPAND` group, used to fold flat `prefix_value` flags back into their
/// `PREFIX="value …"` display column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseExpandGroup {
    /// The flag-name prefix, lowercased with a trailing underscore (for example
    /// `python_targets_`).
    pub prefix: String,
    /// The lowercased group name (for example `python_targets`), uppercased for
    /// display.
    pub name: String,
    /// Whether the group is in `USE_EXPAND_HIDDEN` and should be suppressed.
    pub hidden: bool,
}

/// The prefixed `USE_EXPAND` groups, longest prefix first so the most specific
/// group wins when one prefix is a prefix of another. `USE_EXPAND_UNPREFIXED`
/// variables (whose flags carry no prefix, like `ARCH`) are excluded.
pub fn use_expand_groups(env: &VarMap) -> Vec<UseExpandGroup> {
    let unprefixed: BTreeSet<&str> = tokens(env, "USE_EXPAND_UNPREFIXED").into_iter().collect();
    let hidden_vars: BTreeSet<&str> = tokens(env, "USE_EXPAND_HIDDEN").into_iter().collect();
    let mut groups: Vec<UseExpandGroup> = tokens(env, "USE_EXPAND")
        .into_iter()
        .filter(|var| !unprefixed.contains(var))
        .map(|var| UseExpandGroup {
            prefix: format!("{}_", var.to_lowercase()),
            name: var.to_lowercase(),
            hidden: hidden_vars.contains(var),
        })
        .collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.prefix.len()));
    groups
}

/// Derive `IUSE_EFFECTIVE` for EAPI 5+ from `IUSE_IMPLICIT` and the implicit
/// USE_EXPAND values.
pub fn iuse_effective(env: &VarMap) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = tokens(env, "IUSE_IMPLICIT")
        .into_iter()
        .map(str::to_owned)
        .collect();
    let unprefixed: BTreeSet<&str> = tokens(env, "USE_EXPAND_UNPREFIXED").into_iter().collect();
    for var in tokens(env, "USE_EXPAND_IMPLICIT") {
        for value in tokens(env, var) {
            if unprefixed.contains(var) {
                out.insert(value.to_owned());
            } else {
                out.insert(format!("{}_{}", var.to_lowercase(), value));
            }
        }
    }
    out
}

/// The global USE configuration derived from the environment.
#[derive(Debug, Clone, Default)]
pub struct GlobalUse {
    /// The enabled USE flags.
    pub enabled: Vec<String>,
    /// Flags explicitly disabled by a `-flag`, so they override IUSE `+` defaults.
    pub disabled: BTreeSet<String>,
    /// Flags marked hidden from display.
    pub hidden: BTreeSet<String>,
}

/// Build the global USE set from the environment: USE_EXPAND flattened flags
/// first, then the explicit `USE` value, stacked incrementally.
pub fn global_use(env: &VarMap) -> GlobalUse {
    let (expanded, hidden) = flatten_use_expand(env);
    let use_value = env.get("USE").unwrap_or_default();
    let mut layers: Vec<String> = expanded;
    layers.push(use_value.to_owned());
    let joined: Vec<&str> = layers.iter().map(String::as_str).collect();
    let (enabled, disabled) = stack_layers_signed(joined);
    GlobalUse {
        enabled,
        disabled,
        hidden,
    }
}

/// A single `package.use`-style entry: an atom and its flag modifications.
///
/// For `package.use` the modification flag means "enable"; for
/// `package.use.mask`/`package.use.force` it means "add to the mask/force set"
/// (a `-flag` token clears it again for matching packages).
#[derive(Debug, Clone)]
pub struct PkgUseEntry {
    /// The atom the entry applies to.
    pub atom: Atom,
    /// `(flag, active)` modifications.
    pub mods: Vec<(String, bool)>,
}

/// Resolves effective USE per package from the global set, `package.use`
/// entries, and USE masking and forcing.
#[derive(Debug, Clone, Default)]
pub struct UseManager {
    global: BTreeSet<String>,
    global_disabled: BTreeSet<String>,
    hidden: BTreeSet<String>,
    mask: BTreeSet<String>,
    force: BTreeSet<String>,
    stable_mask: BTreeSet<String>,
    stable_masks_enabled: bool,
    pkg_use: Vec<PkgUseEntry>,
    pkg_mask: Vec<PkgUseEntry>,
    pkg_force: Vec<PkgUseEntry>,
    pkg_stable_mask: Vec<PkgUseEntry>,
    pkg_stable_force: Vec<PkgUseEntry>,
    iuse_effective: BTreeSet<String>,
}

impl UseManager {
    /// Create a manager from the global USE flags and hidden set.
    pub fn new(global: Vec<String>, hidden: BTreeSet<String>) -> Self {
        UseManager {
            global: global.into_iter().collect(),
            hidden,
            ..Self::default()
        }
    }

    /// Set the globally masked flags.
    pub fn with_mask(mut self, mask: impl IntoIterator<Item = String>) -> Self {
        self.mask = mask.into_iter().collect();
        self
    }

    /// Set the globally forced flags.
    pub fn with_force(mut self, force: impl IntoIterator<Item = String>) -> Self {
        self.force = force.into_iter().collect();
        self
    }

    /// Set the flags explicitly disabled in the global USE configuration, which
    /// override IUSE `+` defaults.
    pub fn with_disabled(mut self, disabled: impl IntoIterator<Item = String>) -> Self {
        self.global_disabled = disabled.into_iter().collect();
        self
    }

    /// Set the stable-only masked flags and whether the active EAPI honors them.
    pub fn with_stable_mask(
        mut self,
        mask: impl IntoIterator<Item = String>,
        enabled: bool,
    ) -> Self {
        self.stable_mask = mask.into_iter().collect();
        self.stable_masks_enabled = enabled;
        self
    }

    /// Set the `IUSE_EFFECTIVE` set.
    pub fn with_iuse_effective(mut self, iuse: BTreeSet<String>) -> Self {
        self.iuse_effective = iuse;
        self
    }

    /// Add a `package.use` entry. Entries are applied in specificity order.
    pub fn add_pkg_use(&mut self, entry: PkgUseEntry) {
        self.pkg_use.push(entry);
        self.pkg_use.sort_by_key(|e| specificity(&e.atom));
    }

    /// Add a `package.use.mask` entry. Entries are applied in specificity order.
    pub fn add_pkg_mask(&mut self, entry: PkgUseEntry) {
        self.pkg_mask.push(entry);
        self.pkg_mask.sort_by_key(|e| specificity(&e.atom));
    }

    /// Add a `package.use.force` entry. Entries are applied in specificity order.
    pub fn add_pkg_force(&mut self, entry: PkgUseEntry) {
        self.pkg_force.push(entry);
        self.pkg_force.sort_by_key(|e| specificity(&e.atom));
    }

    /// Add a `package.use.stable.mask` entry, honored only for stable packages.
    pub fn add_pkg_stable_mask(&mut self, entry: PkgUseEntry) {
        self.pkg_stable_mask.push(entry);
        self.pkg_stable_mask.sort_by_key(|e| specificity(&e.atom));
    }

    /// Add a `package.use.stable.force` entry, honored only for stable packages.
    pub fn add_pkg_stable_force(&mut self, entry: PkgUseEntry) {
        self.pkg_stable_force.push(entry);
        self.pkg_stable_force.sort_by_key(|e| specificity(&e.atom));
    }

    /// Whether `flag` is in `IUSE_EFFECTIVE`.
    pub fn is_iuse_effective(&self, flag: &str) -> bool {
        self.iuse_effective.contains(flag)
    }

    /// Compute the effective USE for a package. `stable` selects whether
    /// stable-only masks apply.
    pub fn effective_use(
        &self,
        pkg: &PackageRef<'_>,
        iuse: &[String],
        stable: bool,
    ) -> EffectiveUse {
        // IUSE `+`-prefixed flags are the lowest-priority defaults; global and
        // per-package settings layer on top.
        let mut enabled: BTreeSet<String> = iuse
            .iter()
            .filter_map(|f| f.strip_prefix('+').map(str::to_owned))
            .collect();
        // An explicit `-flag` in the global USE config overrides an IUSE `+`
        // default for that flag.
        for flag in &self.global_disabled {
            enabled.remove(flag);
        }
        enabled.extend(self.global.iter().cloned());

        for entry in &self.pkg_use {
            if entry.atom.matches(pkg) {
                for (flag, enable) in &entry.mods {
                    if *enable {
                        enabled.insert(flag.clone());
                    } else {
                        enabled.remove(flag);
                    }
                }
            }
        }

        // Resolve the effective force and mask sets for this package, layering
        // the per-package entries over the global sets.
        let mut force = resolve_pkg_set(&self.force, &self.pkg_force, pkg);
        let mut mask = resolve_pkg_set(&self.mask, &self.pkg_mask, pkg);
        if stable && self.stable_masks_enabled {
            mask.extend(resolve_pkg_set(
                &self.stable_mask,
                &self.pkg_stable_mask,
                pkg,
            ));
            force.extend(resolve_pkg_set(
                &BTreeSet::new(),
                &self.pkg_stable_force,
                pkg,
            ));
        }

        for flag in &force {
            enabled.insert(flag.clone());
        }
        for flag in &mask {
            enabled.remove(flag);
        }

        let hidden = self.hidden.intersection(&enabled).cloned().collect();
        let forced = force.union(&mask).cloned().collect();
        EffectiveUse {
            enabled,
            hidden,
            forced,
        }
    }
}

/// Resolve a per-package mask/force set: start from the global `base` set and
/// apply the matching per-package entries in specificity order, where an active
/// modification adds the flag and an inactive one (`-flag`) removes it.
fn resolve_pkg_set(
    base: &BTreeSet<String>,
    entries: &[PkgUseEntry],
    pkg: &PackageRef<'_>,
) -> BTreeSet<String> {
    let mut set = base.clone();
    for entry in entries {
        if entry.atom.matches(pkg) {
            for (flag, active) in &entry.mods {
                if *active {
                    set.insert(flag.clone());
                } else {
                    set.remove(flag);
                }
            }
        }
    }
    set
}

/// A specificity score for ordering `package.use` entries (more specific last).
fn specificity(atom: &Atom) -> u32 {
    let mut score = 0;
    if atom.version().is_some() {
        score += 4;
    }
    if atom.slot().is_some() {
        score += 2;
    }
    if atom.repo().is_some() {
        score += 1;
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use moraine_atom::Atom;
    use moraine_common::Interner;
    use moraine_eapi::features_for_level;
    use moraine_version::Version;

    fn env(pairs: &[(&str, &str)]) -> VarMap {
        let mut m = VarMap::new();
        for (k, v) in pairs {
            m.set(*k, *v);
        }
        m
    }

    #[test]
    fn use_expand_prefixed_and_unprefixed() {
        let e = env(&[
            ("USE_EXPAND", "PYTHON_TARGETS ARCH"),
            ("USE_EXPAND_UNPREFIXED", "ARCH"),
            ("PYTHON_TARGETS", "python3_12"),
            ("ARCH", "amd64"),
        ]);
        let (flags, _) = flatten_use_expand(&e);
        assert!(flags.contains(&"python_targets_python3_12".to_owned()));
        assert!(flags.contains(&"amd64".to_owned()));
    }

    #[test]
    fn use_expand_hidden_marked() {
        let e = env(&[
            ("USE_EXPAND", "PYTHON_TARGETS"),
            ("USE_EXPAND_HIDDEN", "PYTHON_TARGETS"),
            ("PYTHON_TARGETS", "python3_12"),
        ]);
        let (_, hidden) = flatten_use_expand(&e);
        assert!(hidden.contains("python_targets_python3_12"));
    }

    #[test]
    fn iuse_effective_includes_implicit_arch() {
        let e = env(&[
            ("USE_EXPAND_IMPLICIT", "ARCH"),
            ("USE_EXPAND_UNPREFIXED", "ARCH"),
            ("ARCH", "amd64"),
        ]);
        assert!(iuse_effective(&e).contains("amd64"));
    }

    fn pkg<'a>(i: &Interner, cat: &str, p: &str, v: &'a Version) -> PackageRef<'a> {
        PackageRef {
            category: i.intern(cat),
            package: i.intern(p),
            version: v,
            slot: None,
            subslot: None,
            repo: None,
        }
    }

    #[test]
    fn iuse_plus_default_is_enabled() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mgr = UseManager::new(vec![], BTreeSet::new());
        let iuse = vec!["+native-symlinks".to_owned(), "test".to_owned()];
        let eff = mgr.effective_use(&pkg(&i, "a", "b", &v), &iuse, false);
        // `+`-prefixed IUSE flags default to enabled; bare ones do not.
        assert!(eff.enabled.contains("native-symlinks"));
        assert!(!eff.enabled.contains("test"));
    }

    #[test]
    fn explicit_disable_overrides_iuse_plus_default() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mgr =
            UseManager::new(vec![], BTreeSet::new()).with_disabled(["native-symlinks".to_owned()]);
        let iuse = vec!["+native-symlinks".to_owned()];
        let eff = mgr.effective_use(&pkg(&i, "a", "b", &v), &iuse, false);
        // A `-flag` in global USE wins over an IUSE `+` default.
        assert!(!eff.enabled.contains("native-symlinks"));
    }

    #[test]
    fn iuse_effective_recognizes_implicit() {
        let mgr = UseManager::new(vec![], BTreeSet::new())
            .with_iuse_effective(["amd64".to_owned()].into_iter().collect());
        assert!(mgr.is_iuse_effective("amd64"));
        assert!(!mgr.is_iuse_effective("ppc"));
    }

    #[test]
    fn pkg_use_overrides_iuse_default() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut mgr = UseManager::new(vec![], BTreeSet::new());
        mgr.add_pkg_use(PkgUseEntry {
            atom: Atom::parse("a/b", moraine_eapi::PERMISSIVE, &i).unwrap(),
            mods: vec![("native-symlinks".to_owned(), false)],
        });
        let iuse = vec!["+native-symlinks".to_owned()];
        let eff = mgr.effective_use(&pkg(&i, "a", "b", &v), &iuse, false);
        assert!(!eff.enabled.contains("native-symlinks"));
    }

    #[test]
    fn mask_and_force() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mgr = UseManager::new(vec!["ssl".into()], BTreeSet::new())
            .with_mask(["ssl".to_owned()])
            .with_force(["forced".to_owned()]);
        let eff = mgr.effective_use(&pkg(&i, "a", "b", &v), &[], true);
        assert!(!eff.enabled.contains("ssl"));
        assert!(eff.enabled.contains("forced"));
    }

    #[test]
    fn stable_mask_only_stable_and_gated() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mgr = UseManager::new(vec!["exp".into()], BTreeSet::new())
            .with_stable_mask(["exp".to_owned()], true);
        assert!(
            !mgr.effective_use(&pkg(&i, "a", "b", &v), &[], true)
                .enabled
                .contains("exp")
        );
        assert!(
            mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false)
                .enabled
                .contains("exp")
        );

        let ungated = UseManager::new(vec!["exp".into()], BTreeSet::new())
            .with_stable_mask(["exp".to_owned()], false);
        assert!(
            ungated
                .effective_use(&pkg(&i, "a", "b", &v), &[], true)
                .enabled
                .contains("exp")
        );
    }

    #[test]
    fn pkg_mask_disables_a_global_flag() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut mgr = UseManager::new(vec!["ssl".into()], BTreeSet::new());
        mgr.add_pkg_mask(PkgUseEntry {
            atom: Atom::parse("a/b", moraine_eapi::PERMISSIVE, &i).unwrap(),
            mods: vec![("ssl".into(), true)],
        });
        // Masked for a/b, still enabled elsewhere.
        assert!(
            !mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false)
                .enabled
                .contains("ssl")
        );
        assert!(
            mgr.effective_use(&pkg(&i, "a", "c", &v), &[], false)
                .enabled
                .contains("ssl")
        );
    }

    #[test]
    fn pkg_force_enables_a_flag() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut mgr = UseManager::new(vec![], BTreeSet::new());
        mgr.add_pkg_force(PkgUseEntry {
            atom: Atom::parse("a/b", moraine_eapi::PERMISSIVE, &i).unwrap(),
            mods: vec![("forced".into(), true)],
        });
        assert!(
            mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false)
                .enabled
                .contains("forced")
        );
        assert!(
            !mgr.effective_use(&pkg(&i, "a", "c", &v), &[], false)
                .enabled
                .contains("forced")
        );
    }

    #[test]
    fn pkg_stable_mask_only_for_stable() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mut mgr = UseManager::new(vec!["exp".into()], BTreeSet::new())
            .with_stable_mask(Vec::<String>::new(), true);
        mgr.add_pkg_stable_mask(PkgUseEntry {
            atom: Atom::parse("a/b", moraine_eapi::PERMISSIVE, &i).unwrap(),
            mods: vec![("exp".into(), true)],
        });
        assert!(
            !mgr.effective_use(&pkg(&i, "a", "b", &v), &[], true)
                .enabled
                .contains("exp")
        );
        assert!(
            mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false)
                .enabled
                .contains("exp")
        );
    }

    #[test]
    fn package_use_overrides_and_specificity() {
        let i = Interner::new();
        let f = features_for_level(8);
        let mut mgr = UseManager::new(vec![], BTreeSet::new());
        mgr.add_pkg_use(PkgUseEntry {
            atom: Atom::parse("dev-libs/foo", f, &i).unwrap(),
            mods: vec![("ssl".into(), true)],
        });
        mgr.add_pkg_use(PkgUseEntry {
            atom: Atom::parse(">=dev-libs/foo-2", f, &i).unwrap(),
            mods: vec![("ssl".into(), false)],
        });
        let v = Version::parse("2.0").unwrap();
        let eff = mgr.effective_use(&pkg(&i, "dev-libs", "foo", &v), &[], true);
        // The more specific versioned entry (applied last) disables ssl.
        assert!(!eff.enabled.contains("ssl"));
    }
}
