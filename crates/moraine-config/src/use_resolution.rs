//! Effective USE computation: USE_EXPAND flattening, masking and forcing,
//! `package.use` overrides, and `IUSE_EFFECTIVE` derivation.

use std::collections::{BTreeSet, HashMap};

use moraine_atom::{Atom, PackageRef};
use moraine_common::Symbol;

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

/// The full valid-value set for a USE_EXPAND_IMPLICIT variable, read from its
/// profile-stacked `USE_EXPAND_VALUES_<VAR>` set (`config.py:2337,2344`). When
/// that set is unset the loader's `PORTAGE_ARCHLIST` is used for `ARCH`, and as a
/// last resort the variable's own current value, so a single configured value
/// still contributes.
fn use_expand_values<'a>(env: &'a VarMap, var: &str) -> Vec<&'a str> {
    let key = format!("USE_EXPAND_VALUES_{var}");
    let direct = tokens(env, &key);
    if !direct.is_empty() {
        return direct;
    }
    if var == "ARCH" {
        let archlist = tokens(env, "PORTAGE_ARCHLIST");
        if !archlist.is_empty() {
            return archlist;
        }
    }
    tokens(env, var)
}

/// Derive `IUSE_EFFECTIVE` for EAPI 5+ from `IUSE_IMPLICIT` and, for each
/// USE_EXPAND_IMPLICIT variable, every value in its `USE_EXPAND_VALUES_*` set, so
/// implicit flags for all valid values (not only the selected one) are
/// recognized as valid, mirroring `config._calc_iuse_effective`.
pub fn iuse_effective(env: &VarMap) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = tokens(env, "IUSE_IMPLICIT")
        .into_iter()
        .map(str::to_owned)
        .collect();
    let implicit: BTreeSet<&str> = tokens(env, "USE_EXPAND_IMPLICIT").into_iter().collect();
    // Unprefixed implicit variables (at least ARCH) contribute bare values.
    for var in tokens(env, "USE_EXPAND_UNPREFIXED") {
        if implicit.contains(var) {
            for value in use_expand_values(env, var) {
                out.insert(value.to_owned());
            }
        }
    }
    // Prefixed implicit variables contribute `var_value` flags.
    let use_expand: BTreeSet<&str> = tokens(env, "USE_EXPAND").into_iter().collect();
    for var in &implicit {
        if use_expand.contains(var) {
            for value in use_expand_values(env, var) {
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

/// Which configuration layer a per-package entry comes from. The profile layer
/// is always applied before the user (`/etc/portage`) layer, regardless of
/// cross-layer specificity, mirroring Portage's `defaults` then `pkg` config
/// layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseLayer {
    /// A profile-node entry (applied first).
    Profile,
    /// A user `/etc/portage` entry (applied last).
    User,
}

/// Per-package entries split into a profile layer and a user layer, each ordered
/// by atom specificity within itself. The profile layer is applied before the
/// user layer so a less specific user entry still overrides a more specific
/// profile entry.
#[derive(Debug, Clone, Default)]
struct LayeredPkgEntries {
    profile: Vec<PkgUseEntry>,
    user: Vec<PkgUseEntry>,
}

impl LayeredPkgEntries {
    fn add(&mut self, entry: PkgUseEntry, layer: UseLayer) {
        let bucket = match layer {
            UseLayer::Profile => &mut self.profile,
            UseLayer::User => &mut self.user,
        };
        bucket.push(entry);
        bucket.sort_by_key(|e| specificity(&e.atom));
    }

    /// The entries to apply, profile layer first (specificity-ordered), then the
    /// user layer (specificity-ordered).
    fn applied(&self) -> impl Iterator<Item = &PkgUseEntry> {
        self.profile.iter().chain(self.user.iter())
    }
}

/// One mask-or-force stream within a profile node: the node's global signed
/// tokens (and the stable-gated variant) paired with its per-package entries (and
/// the stable-gated variant), each specificity-ordered within the node.
#[derive(Debug, Clone, Default)]
struct NodeSigned {
    global: Vec<String>,
    stable_global: Vec<String>,
    pkg: Vec<PkgUseEntry>,
    stable_pkg: Vec<PkgUseEntry>,
}

impl NodeSigned {
    fn add_pkg(&mut self, entry: PkgUseEntry) {
        self.pkg.push(entry);
        self.pkg.sort_by_key(|e| specificity(&e.atom));
    }

    fn add_stable_pkg(&mut self, entry: PkgUseEntry) {
        self.stable_pkg.push(entry);
        self.stable_pkg.sort_by_key(|e| specificity(&e.atom));
    }
}

/// One profile node's (or the user `/etc/portage` node's) USE masking and forcing
/// inputs. Each node pairs its global `use.mask`/`use.force` signed tokens (and the
/// stable variants) with its `package.use.mask`/`package.use.force` per-package
/// entries, so the global file and its per-package companions interleave in node
/// order rather than every node's global file collapsing first, mirroring
/// `UseManager.getUseMask`/`getUseForce`.
#[derive(Debug, Clone, Default)]
pub struct ProfileUseNode {
    mask: NodeSigned,
    force: NodeSigned,
}

impl ProfileUseNode {
    /// Create an empty node.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the node's global `use.mask` signed tokens.
    pub fn set_mask(&mut self, tokens: Vec<String>) {
        self.mask.global = tokens;
    }

    /// Set the node's global `use.force` signed tokens.
    pub fn set_force(&mut self, tokens: Vec<String>) {
        self.force.global = tokens;
    }

    /// Set the node's global `use.stable.mask` signed tokens, honored only for
    /// stable packages.
    pub fn set_stable_mask(&mut self, tokens: Vec<String>) {
        self.mask.stable_global = tokens;
    }

    /// Set the node's global `use.stable.force` signed tokens, honored only for
    /// stable packages.
    pub fn set_stable_force(&mut self, tokens: Vec<String>) {
        self.force.stable_global = tokens;
    }

    /// Add a `package.use.mask` entry to the node.
    pub fn add_pkg_mask(&mut self, entry: PkgUseEntry) {
        self.mask.add_pkg(entry);
    }

    /// Add a `package.use.force` entry to the node.
    pub fn add_pkg_force(&mut self, entry: PkgUseEntry) {
        self.force.add_pkg(entry);
    }

    /// Add a `package.use.stable.mask` entry to the node, honored only for stable
    /// packages.
    pub fn add_pkg_stable_mask(&mut self, entry: PkgUseEntry) {
        self.mask.add_stable_pkg(entry);
    }

    /// Add a `package.use.stable.force` entry to the node, honored only for stable
    /// packages.
    pub fn add_pkg_stable_force(&mut self, entry: PkgUseEntry) {
        self.force.add_stable_pkg(entry);
    }
}

/// Fold a list of signed flag tokens onto `set`: a `-flag` token removes a prior
/// entry, any other token inserts the flag, mirroring `stack_lists(incremental)`.
fn apply_tokens(set: &mut BTreeSet<String>, tokens: &[String]) {
    for token in tokens {
        if let Some(rest) = token.strip_prefix('-') {
            set.remove(rest);
        } else {
            set.insert(token.clone());
        }
    }
}

/// Fold a per-package entry's modifications onto `set`: an active flag is added,
/// an inactive (`-flag`) one is removed.
fn apply_entry(set: &mut BTreeSet<String>, entry: &PkgUseEntry) {
    for (flag, active) in &entry.mods {
        if *active {
            set.insert(flag.clone());
        } else {
            set.remove(flag);
        }
    }
}

/// Resolves effective USE per package from the global set, `package.use`
/// entries, and USE masking and forcing.
#[derive(Debug, Clone, Default)]
pub struct UseManager {
    global: BTreeSet<String>,
    global_disabled: BTreeSet<String>,
    hidden: BTreeSet<String>,
    // The profile architecture keyword (for example `amd64`), added to a
    // package's effective USE after force is applied and before mask removal.
    arch: Option<String>,
    features_test: bool,
    pkg_use: LayeredPkgEntries,
    // The profile mask/force nodes in stack order, with the user `/etc/portage`
    // node appended last. Each node's global file interleaves with its
    // per-package entries when force and mask are resolved.
    nodes: Vec<ProfileUseNode>,
    // Repository-level USE configuration, keyed by the owning repository symbol
    // (its flag stack already folds in the masters). Applied beneath the profile
    // cascade, scoped to candidates from the owning repository.
    repo_mask: HashMap<Symbol, BTreeSet<String>>,
    repo_force: HashMap<Symbol, BTreeSet<String>>,
    repo_stable_mask: HashMap<Symbol, BTreeSet<String>>,
    repo_pkg_use: Vec<(Symbol, PkgUseEntry)>,
    repo_pkg_mask: Vec<(Symbol, PkgUseEntry)>,
    repo_pkg_force: Vec<(Symbol, PkgUseEntry)>,
    repo_pkg_stable_mask: Vec<(Symbol, PkgUseEntry)>,
    repo_pkg_stable_force: Vec<(Symbol, PkgUseEntry)>,
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

    /// Set the profile architecture keyword. An empty value records no arch.
    pub fn with_arch(mut self, arch: impl Into<String>) -> Self {
        let arch = arch.into();
        self.arch = (!arch.is_empty()).then_some(arch);
        self
    }

    /// Set the profile mask/force nodes in stack order, with the user
    /// `/etc/portage` node appended last.
    pub fn with_nodes(mut self, nodes: Vec<ProfileUseNode>) -> Self {
        self.nodes = nodes;
        self
    }

    /// Set the flags explicitly disabled in the global USE configuration, which
    /// override IUSE `+` defaults.
    pub fn with_disabled(mut self, disabled: impl IntoIterator<Item = String>) -> Self {
        self.global_disabled = disabled.into_iter().collect();
        self
    }

    /// Set the `IUSE_EFFECTIVE` set.
    pub fn with_iuse_effective(mut self, iuse: BTreeSet<String>) -> Self {
        self.iuse_effective = iuse;
        self
    }

    /// Record whether `FEATURES` contains `test`, so the `test` USE flag is
    /// injected for packages that do not restrict testing.
    pub fn with_features_test(mut self, enabled: bool) -> Self {
        self.features_test = enabled;
        self
    }

    /// Add a `package.use` entry from the given layer. Entries within a layer are
    /// applied in specificity order; the profile layer is applied before the user
    /// layer.
    pub fn add_pkg_use(&mut self, entry: PkgUseEntry, layer: UseLayer) {
        self.pkg_use.add(entry, layer);
    }

    /// Set the repository-level `use.mask` flags for `repo` (applied to that
    /// repository's candidates).
    pub fn add_repo_use_mask(&mut self, repo: Symbol, flags: impl IntoIterator<Item = String>) {
        self.repo_mask.entry(repo).or_default().extend(flags);
    }

    /// Set the repository-level `use.force` flags for `repo`.
    pub fn add_repo_use_force(&mut self, repo: Symbol, flags: impl IntoIterator<Item = String>) {
        self.repo_force.entry(repo).or_default().extend(flags);
    }

    /// Set the repository-level `use.stable.mask` flags for `repo`.
    pub fn add_repo_use_stable_mask(
        &mut self,
        repo: Symbol,
        flags: impl IntoIterator<Item = String>,
    ) {
        self.repo_stable_mask.entry(repo).or_default().extend(flags);
    }

    /// Add a repository-level `package.use` entry scoped to `repo`.
    pub fn add_repo_pkg_use(&mut self, repo: Symbol, entry: PkgUseEntry) {
        self.repo_pkg_use.push((repo, entry));
    }

    /// Add a repository-level `package.use.mask` entry scoped to `repo`.
    pub fn add_repo_pkg_mask(&mut self, repo: Symbol, entry: PkgUseEntry) {
        self.repo_pkg_mask.push((repo, entry));
    }

    /// Add a repository-level `package.use.force` entry scoped to `repo`.
    pub fn add_repo_pkg_force(&mut self, repo: Symbol, entry: PkgUseEntry) {
        self.repo_pkg_force.push((repo, entry));
    }

    /// Add a repository-level `package.use.stable.mask` entry scoped to `repo`.
    pub fn add_repo_pkg_stable_mask(&mut self, repo: Symbol, entry: PkgUseEntry) {
        self.repo_pkg_stable_mask.push((repo, entry));
    }

    /// Add a repository-level `package.use.stable.force` entry scoped to `repo`.
    pub fn add_repo_pkg_stable_force(&mut self, repo: Symbol, entry: PkgUseEntry) {
        self.repo_pkg_stable_force.push((repo, entry));
    }

    /// Whether `flag` is in `IUSE_EFFECTIVE`.
    pub fn is_iuse_effective(&self, flag: &str) -> bool {
        self.iuse_effective.contains(flag)
    }

    /// Compute the effective USE for a package. `stable` selects whether
    /// stable-only masks apply. `restrict_test` is whether the package's
    /// `RESTRICT` contains `test`, which suppresses the `FEATURES=test`
    /// injection.
    pub fn effective_use(
        &self,
        pkg: &PackageRef<'_>,
        iuse: &[String],
        stable: bool,
        restrict_test: bool,
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

        // Repository-level `package.use` is applied beneath the profile cascade,
        // scoped to candidates from the owning repository.
        apply_repo_pkg(&self.repo_pkg_use, pkg, &mut enabled);
        for entry in self.pkg_use.applied() {
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

        // FEATURES=test injects `test` (independent of IUSE) before mask/force,
        // re-disabled when the package RESTRICT contains `test`, mirroring
        // `config._setcpv`.
        if self.features_test && !restrict_test {
            enabled.insert("test".to_owned());
        }

        // Resolve the effective force and mask sets for this package by walking
        // the repository layers first, then the per-node stream in node order.
        let force = self.resolve_layer(
            pkg,
            stable,
            &self.repo_force,
            None,
            &self.repo_pkg_force,
            &self.repo_pkg_stable_force,
            |node| &node.force,
        );
        let mask = self.resolve_layer(
            pkg,
            stable,
            &self.repo_mask,
            Some(&self.repo_stable_mask),
            &self.repo_pkg_mask,
            &self.repo_pkg_stable_mask,
            |node| &node.mask,
        );

        for flag in &force {
            enabled.insert(flag.clone());
        }
        // The profile arch keyword is added after force and before mask removal,
        // mirroring `config.regenerate` (`config.py:3037-3039`): it cannot be
        // disabled by `package.use` but a mask still removes it.
        if let Some(arch) = &self.arch {
            enabled.insert(arch.clone());
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

/// Apply repository-level `package.use` entries scoped to the candidate's
/// repository onto the enabled set.
fn apply_repo_pkg(
    entries: &[(Symbol, PkgUseEntry)],
    pkg: &PackageRef<'_>,
    enabled: &mut BTreeSet<String>,
) {
    for (repo, entry) in entries {
        if pkg.repo == Some(*repo) && entry.atom.matches(pkg) {
            for (flag, enable) in &entry.mods {
                if *enable {
                    enabled.insert(flag.clone());
                } else {
                    enabled.remove(flag);
                }
            }
        }
    }
}

impl UseManager {
    /// Resolve a per-package mask or force set as a single incremental stream:
    /// apply the repository layers scoped to the candidate's repository first
    /// (the flag stack already folded the masters), then walk the per-node stream
    /// in node order, applying each node's global tokens, its stable global tokens
    /// (when `stable`), its matching per-package entries (specificity-ordered), and
    /// its stable per-package entries (when `stable`). `select` picks the node's
    /// mask or force stream. This mirrors `UseManager.getUseMask`/`getUseForce`.
    #[allow(clippy::too_many_arguments)]
    fn resolve_layer(
        &self,
        pkg: &PackageRef<'_>,
        stable: bool,
        repo_global: &HashMap<Symbol, BTreeSet<String>>,
        repo_stable_global: Option<&HashMap<Symbol, BTreeSet<String>>>,
        repo_entries: &[(Symbol, PkgUseEntry)],
        repo_stable_entries: &[(Symbol, PkgUseEntry)],
        select: impl Fn(&ProfileUseNode) -> &NodeSigned,
    ) -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        if let Some(r) = pkg.repo {
            if let Some(flags) = repo_global.get(&r) {
                set.extend(flags.iter().cloned());
            }
            if stable && let Some(flags) = repo_stable_global.and_then(|m| m.get(&r)) {
                set.extend(flags.iter().cloned());
            }
        }
        for (repo, entry) in repo_entries {
            if pkg.repo == Some(*repo) && entry.atom.matches(pkg) {
                apply_entry(&mut set, entry);
            }
        }
        if stable {
            for (repo, entry) in repo_stable_entries {
                if pkg.repo == Some(*repo) && entry.atom.matches(pkg) {
                    apply_entry(&mut set, entry);
                }
            }
        }
        for node in &self.nodes {
            let signed = select(node);
            apply_tokens(&mut set, &signed.global);
            if stable {
                apply_tokens(&mut set, &signed.stable_global);
            }
            for entry in &signed.pkg {
                if entry.atom.matches(pkg) {
                    apply_entry(&mut set, entry);
                }
            }
            if stable {
                for entry in &signed.stable_pkg {
                    if entry.atom.matches(pkg) {
                        apply_entry(&mut set, entry);
                    }
                }
            }
        }
        set
    }
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
        // IUSE_EFFECTIVE is derived from USE_EXPAND_VALUES_ARCH, so every valid
        // arch value is included, not only the selected one.
        let e = env(&[
            ("USE_EXPAND_IMPLICIT", "ARCH"),
            ("USE_EXPAND_UNPREFIXED", "ARCH"),
            ("ARCH", "amd64"),
            ("USE_EXPAND_VALUES_ARCH", "amd64 x86 arm64"),
        ]);
        let eff = iuse_effective(&e);
        assert!(eff.contains("amd64"));
        // Non-selected arch values are part of IUSE_EFFECTIVE.
        assert!(eff.contains("x86"));
        assert!(eff.contains("arm64"));
    }

    #[test]
    fn iuse_effective_arch_falls_back_to_archlist() {
        // With no explicit USE_EXPAND_VALUES_ARCH, the loader's PORTAGE_ARCHLIST
        // supplies the full arch value set.
        let e = env(&[
            ("USE_EXPAND_IMPLICIT", "ARCH"),
            ("USE_EXPAND_UNPREFIXED", "ARCH"),
            ("ARCH", "amd64"),
            ("PORTAGE_ARCHLIST", "amd64 x86 ppc"),
        ]);
        let eff = iuse_effective(&e);
        assert!(eff.contains("amd64") && eff.contains("x86") && eff.contains("ppc"));
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
        let eff = mgr.effective_use(&pkg(&i, "a", "b", &v), &iuse, false, false);
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
        let eff = mgr.effective_use(&pkg(&i, "a", "b", &v), &iuse, false, false);
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
        mgr.add_pkg_use(
            PkgUseEntry {
                atom: Atom::parse("a/b", moraine_eapi::PERMISSIVE, &i).unwrap(),
                mods: vec![("native-symlinks".to_owned(), false)],
            },
            UseLayer::User,
        );
        let iuse = vec!["+native-symlinks".to_owned()];
        let eff = mgr.effective_use(&pkg(&i, "a", "b", &v), &iuse, false, false);
        assert!(!eff.enabled.contains("native-symlinks"));
    }

    fn node_with(build: impl FnOnce(&mut ProfileUseNode)) -> ProfileUseNode {
        let mut node = ProfileUseNode::new();
        build(&mut node);
        node
    }

    #[test]
    fn arch_flag_is_enabled_and_maskable() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        // The profile arch keyword is always an enabled USE flag.
        let mgr = UseManager::new(vec![], BTreeSet::new()).with_arch("amd64");
        let eff = mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false, false);
        assert!(eff.enabled.contains("amd64"));

        // A use.mask of the arch flag still removes it.
        let masked = UseManager::new(vec![], BTreeSet::new())
            .with_arch("amd64")
            .with_nodes(vec![node_with(|n| n.set_mask(vec!["amd64".to_owned()]))]);
        let eff = masked.effective_use(&pkg(&i, "a", "b", &v), &[], false, false);
        assert!(!eff.enabled.contains("amd64"));
    }

    #[test]
    fn mask_and_force() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mgr =
            UseManager::new(vec!["ssl".into()], BTreeSet::new()).with_nodes(vec![node_with(|n| {
                n.set_mask(vec!["ssl".to_owned()]);
                n.set_force(vec!["forced".to_owned()]);
            })]);
        let eff = mgr.effective_use(&pkg(&i, "a", "b", &v), &[], true, false);
        assert!(!eff.enabled.contains("ssl"));
        assert!(eff.enabled.contains("forced"));
    }

    #[test]
    fn stable_mask_only_stable() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mgr =
            UseManager::new(vec!["exp".into()], BTreeSet::new()).with_nodes(vec![node_with(|n| {
                n.set_stable_mask(vec!["exp".to_owned()])
            })]);
        // The stable mask applies to a stable package but not a testing one.
        assert!(
            !mgr.effective_use(&pkg(&i, "a", "b", &v), &[], true, false)
                .enabled
                .contains("exp")
        );
        assert!(
            mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false, false)
                .enabled
                .contains("exp")
        );
    }

    #[test]
    fn global_stable_force_only_stable() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mgr = UseManager::new(vec![], BTreeSet::new()).with_nodes(vec![node_with(|n| {
            n.set_stable_force(vec!["secure".to_owned()])
        })]);
        // use.stable.force forces the flag for stable packages only.
        assert!(
            mgr.effective_use(&pkg(&i, "a", "b", &v), &[], true, false)
                .enabled
                .contains("secure")
        );
        assert!(
            !mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false, false)
                .enabled
                .contains("secure")
        );
    }

    #[test]
    fn pkg_mask_disables_a_global_flag() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mgr =
            UseManager::new(vec!["ssl".into()], BTreeSet::new()).with_nodes(vec![node_with(|n| {
                n.add_pkg_mask(PkgUseEntry {
                    atom: Atom::parse("a/b", moraine_eapi::PERMISSIVE, &i).unwrap(),
                    mods: vec![("ssl".into(), true)],
                });
            })]);
        // Masked for a/b, still enabled elsewhere.
        assert!(
            !mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false, false)
                .enabled
                .contains("ssl")
        );
        assert!(
            mgr.effective_use(&pkg(&i, "a", "c", &v), &[], false, false)
                .enabled
                .contains("ssl")
        );
    }

    #[test]
    fn pkg_force_enables_a_flag() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mgr = UseManager::new(vec![], BTreeSet::new()).with_nodes(vec![node_with(|n| {
            n.add_pkg_force(PkgUseEntry {
                atom: Atom::parse("a/b", moraine_eapi::PERMISSIVE, &i).unwrap(),
                mods: vec![("forced".into(), true)],
            });
        })]);
        assert!(
            mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false, false)
                .enabled
                .contains("forced")
        );
        assert!(
            !mgr.effective_use(&pkg(&i, "a", "c", &v), &[], false, false)
                .enabled
                .contains("forced")
        );
    }

    #[test]
    fn pkg_stable_mask_only_for_stable() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mgr =
            UseManager::new(vec!["exp".into()], BTreeSet::new()).with_nodes(vec![node_with(|n| {
                n.add_pkg_stable_mask(PkgUseEntry {
                    atom: Atom::parse("a/b", moraine_eapi::PERMISSIVE, &i).unwrap(),
                    mods: vec![("exp".into(), true)],
                });
            })]);
        assert!(
            !mgr.effective_use(&pkg(&i, "a", "b", &v), &[], true, false)
                .enabled
                .contains("exp")
        );
        assert!(
            mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false, false)
                .enabled
                .contains("exp")
        );
    }

    #[test]
    fn later_node_global_mask_pops_earlier_pkg_mask() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        // A base node masks `clang` for a/b via package.use.mask; a later node's
        // global use.mask `-clang` pops that mask.
        let base = node_with(|n| {
            n.add_pkg_mask(PkgUseEntry {
                atom: Atom::parse("a/b", moraine_eapi::PERMISSIVE, &i).unwrap(),
                mods: vec![("clang".into(), true)],
            });
        });
        let child = node_with(|n| n.set_mask(vec!["-clang".to_owned()]));
        let mgr =
            UseManager::new(vec!["clang".into()], BTreeSet::new()).with_nodes(vec![base, child]);
        assert!(
            mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false, false)
                .enabled
                .contains("clang")
        );
    }

    #[test]
    fn package_use_overrides_and_specificity() {
        let i = Interner::new();
        let f = features_for_level(8);
        let mut mgr = UseManager::new(vec![], BTreeSet::new());
        mgr.add_pkg_use(
            PkgUseEntry {
                atom: Atom::parse("dev-libs/foo", f, &i).unwrap(),
                mods: vec![("ssl".into(), true)],
            },
            UseLayer::User,
        );
        mgr.add_pkg_use(
            PkgUseEntry {
                atom: Atom::parse(">=dev-libs/foo-2", f, &i).unwrap(),
                mods: vec![("ssl".into(), false)],
            },
            UseLayer::User,
        );
        let v = Version::parse("2.0").unwrap();
        let eff = mgr.effective_use(&pkg(&i, "dev-libs", "foo", &v), &[], true, false);
        // The more specific versioned entry (applied last) disables ssl.
        assert!(!eff.enabled.contains("ssl"));
    }

    #[test]
    fn user_layer_overrides_more_specific_profile_entry() {
        let i = Interner::new();
        let f = features_for_level(8);
        let mut mgr = UseManager::new(vec![], BTreeSet::new());
        // A more specific profile entry enables foo.
        mgr.add_pkg_use(
            PkgUseEntry {
                atom: Atom::parse("=cat/pkg-1.0", f, &i).unwrap(),
                mods: vec![("foo".into(), true)],
            },
            UseLayer::Profile,
        );
        // A less specific user entry disables it.
        mgr.add_pkg_use(
            PkgUseEntry {
                atom: Atom::parse("cat/pkg", f, &i).unwrap(),
                mods: vec![("foo".into(), false)],
            },
            UseLayer::User,
        );
        let v = Version::parse("1.0").unwrap();
        let eff = mgr.effective_use(&pkg(&i, "cat", "pkg", &v), &[], false, false);
        // The user layer is applied after the profile layer, so foo is disabled
        // even though the profile entry's atom is more specific.
        assert!(!eff.enabled.contains("foo"));
    }

    #[test]
    fn features_test_injects_test_flag() {
        let i = Interner::new();
        let v = Version::parse("1.0").unwrap();
        let mgr = UseManager::new(vec![], BTreeSet::new()).with_features_test(true);
        // FEATURES=test injects `test` independently of IUSE.
        let eff = mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false, false);
        assert!(eff.enabled.contains("test"));
        // RESTRICT=test re-disables the injected flag.
        let eff = mgr.effective_use(&pkg(&i, "a", "b", &v), &[], false, true);
        assert!(!eff.enabled.contains("test"));
    }
}
