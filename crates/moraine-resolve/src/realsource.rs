//! A [`ResolveSource`] backed by the real `moraine-repo`, `moraine-vdb`, and
//! `moraine-config` crates.
//!
//! Each backing store interns identifiers against its own interner, so this
//! adapter resolves every token to an owned string at the boundary. Candidate
//! metadata, visibility, USE, `package.provided`, and installed state all flow
//! through the [`ResolveSource`] trait so the provider and encoder never touch a
//! foreign interner.

use std::collections::{BTreeSet, HashMap};

use moraine_atom::PackageRef;
use moraine_binpkg::resolution::{BinaryCandidate, TargetConfig, Verdict, check_compatibility};
use moraine_config::ResolvedConfig;
use moraine_repo::RepoIndex;
use moraine_vdb::Store;
use moraine_version::Version;

use crate::depnode::{BlockerKind, DepNode};
use crate::normalize::normalize_depspec;
use crate::required_use::parse_required_use;
use crate::source::{AcceptChange, Acceptability, InstalledMeta, PackageMeta, ResolveSource};

/// The new-style virtual whose installed providers are the system libc, stripped
/// from the `--changed-deps` comparison (Portage's `LIBC_PACKAGE_ATOM`).
const LIBC_PACKAGE_ATOM: &str = "virtual/libc";

/// A resolution source over a repository index, installed store, and resolved
/// configuration.
pub struct RealSource<'a> {
    repo: &'a RepoIndex,
    vdb: &'a Store,
    config: &'a ResolvedConfig,
    /// Whether to evaluate keyword acceptance as stable.
    stable: bool,
    /// Binary candidates keyed by `cp-version`, so version selection can prefer a
    /// compatible binary. Empty when `getbinpkg`/`usepkg` is off.
    binaries: HashMap<String, BinaryCandidate>,
    /// The target configuration binary candidates are checked against. When
    /// `None`, a present candidate is offered on cpv presence alone.
    binary_target: Option<TargetConfig>,
}

impl<'a> RealSource<'a> {
    /// Create a real source over the given backing stores.
    pub fn new(repo: &'a RepoIndex, vdb: &'a Store, config: &'a ResolvedConfig) -> Self {
        RealSource {
            repo,
            vdb,
            config,
            stable: false,
            binaries: HashMap::new(),
            binary_target: None,
        }
    }

    /// Set whether keyword acceptance is evaluated as stable.
    pub fn with_stable(mut self, stable: bool) -> Self {
        self.stable = stable;
        self
    }

    /// Provide the binary candidates (keyed by `cp-version`) and the optional
    /// target configuration they are checked against, so version selection
    /// prefers a compatible binary under `getbinpkg`/`usepkg`.
    pub fn with_binaries(
        mut self,
        binaries: HashMap<String, BinaryCandidate>,
        target: Option<TargetConfig>,
    ) -> Self {
        self.binaries = binaries;
        self.binary_target = target;
        self
    }

    /// Split a `category/package` into its parts.
    fn split_cp(cp: &str) -> Option<(&str, &str)> {
        cp.split_once('/')
    }

    /// Convert a vdb record (with its already-resolved `cp`) into an
    /// [`InstalledMeta`], resolving slot, USE, IUSE, and `:=` bindings.
    fn record_to_installed(
        &self,
        cp: String,
        record: &moraine_vdb::PackageRecord,
    ) -> InstalledMeta {
        let interner = self.vdb.interner();
        let (slot_sym, subslot_sym) = self.vdb.recorded_slot(record);
        let slot = interner
            .resolve(slot_sym)
            .map(|s| s.to_string())
            .unwrap_or_default();
        let subslot = subslot_sym
            .and_then(|s| interner.resolve(s))
            .map(|s| s.to_string());
        let use_enabled = self
            .vdb
            .recorded_use(record)
            .iter()
            .filter_map(|s| interner.resolve(*s).map(|x| x.to_string()))
            .collect();
        let iuse = record
            .iuse
            .iter()
            .map(|f| f.trim_start_matches(['+', '-']).to_owned())
            .collect();
        let slot_bindings = self
            .vdb
            .slot_operator_bindings(record)
            .into_iter()
            .filter_map(|b| {
                let c = interner.resolve(b.category)?;
                let p = interner.resolve(b.package)?;
                let bslot = b
                    .slot
                    .and_then(|s| interner.resolve(s))
                    .map(|s| s.to_string());
                let bsub = b
                    .subslot
                    .and_then(|s| interner.resolve(s))
                    .map(|s| s.to_string());
                Some((format!("{c}/{p}"), bslot.unwrap_or_default(), bsub))
            })
            .collect();
        let recorded_deps = moraine_vdb::record::DependKind::ALL
            .iter()
            .filter_map(|kind| {
                record
                    .depends
                    .get(*kind)
                    .map(|dep| (kind.name().to_owned(), dep.raw.clone()))
            })
            .collect();
        InstalledMeta {
            cp,
            version: record.version.clone(),
            slot,
            subslot,
            use_enabled,
            iuse,
            slot_bindings,
            recorded_deps,
        }
    }
}

/// Whether the binary candidate keyed by `key` is offered for selection.
///
/// A candidate is offered only when it is present and (with a `target` present)
/// passes [`check_compatibility`]; an incompatible binary (foreign CHOST,
/// mismatched USE, or unsatisfied sonames) is not offered, so the ebuild
/// candidate is used instead.
fn binary_offered(
    binaries: &HashMap<String, BinaryCandidate>,
    target: Option<&TargetConfig>,
    key: &str,
) -> bool {
    let Some(candidate) = binaries.get(key) else {
        return false;
    };
    match target {
        None => true,
        Some(target) => check_compatibility(candidate, target) == Verdict::Accept,
    }
}

impl ResolveSource for RealSource<'_> {
    fn versions_of(&self, cp: &str) -> Vec<PackageMeta> {
        let mut out = Vec::new();
        // Match every version of the cp across repos via a bare atom.
        for cand in self.repo.match_atom_str(cp) {
            let store = &self.repo.repos()[cand.repo_order].store;
            let interner = store.interner();
            let entry = cand.entry;
            let cp_str = {
                let category = interner.resolve(entry.category);
                let package = interner.resolve(entry.package);
                match (category, package) {
                    (Some(c), Some(p)) => format!("{c}/{p}"),
                    _ => continue,
                }
            };
            if cp_str != cp {
                continue;
            }
            let slot = interner
                .resolve(entry.slot)
                .map(|s| s.to_string())
                .unwrap_or_default();
            let subslot = entry
                .subslot
                .and_then(|s| interner.resolve(s))
                .map(|s| s.to_string());
            // Bare flag names, with the `+`/`-` IUSE default prefix stripped, so
            // membership checks work; defaults are applied in `resolved_use`.
            let iuse = entry
                .iuse
                .iter()
                .filter_map(|s| {
                    interner
                        .resolve(*s)
                        .map(|x| x.trim_start_matches(['+', '-']).to_string())
                })
                .collect();
            out.push(PackageMeta {
                cp: cp_str,
                version: entry.version.clone(),
                eapi: entry.eapi.clone(),
                slot,
                subslot,
                depend: normalize_depspec(&entry.depend, interner),
                bdepend: normalize_depspec(&entry.bdepend, interner),
                rdepend: normalize_depspec(&entry.rdepend, interner),
                pdepend: normalize_depspec(&entry.pdepend, interner),
                idepend: normalize_depspec(&entry.idepend, interner),
                // REQUIRED_USE leaves are USE flags, not atoms; parse the raw
                // text with the dedicated USE-constraint parser.
                required_use: parse_required_use(&entry.required_use),
                license: entry.license.clone(),
                iuse,
            });
        }
        out.sort_by(|a, b| a.version.cmp(&b.version));
        out
    }

    fn is_visible(&self, meta: &PackageMeta) -> bool {
        let (category, package) = match Self::split_cp(&meta.cp) {
            Some(p) => p,
            None => return false,
        };
        // Build a PackageRef against the config's namespace. The config queries
        // use Symbols from its profile's interner; we intern through the repo's
        // first store interner for the slot symbols. To keep the boundary
        // simple, look up the original entry to reuse its interned symbols.
        for cand in self.repo.match_atom_str(&meta.cp) {
            let store = &self.repo.repos()[cand.repo_order].store;
            let interner = store.interner();
            let entry = cand.entry;
            if entry.version != meta.version {
                continue;
            }
            let pref = PackageRef {
                category: interner.intern(category),
                package: interner.intern(package),
                version: &entry.version,
                slot: Some(entry.slot),
                subslot: entry.subslot,
                repo: Some(entry.repository),
            };
            let ebuild_keywords: Vec<String> = entry
                .keywords
                .iter()
                .filter_map(|k| interner.resolve(*k).map(|x| x.to_string()))
                .collect();
            // Apply profile `package.keywords` to the ebuild KEYWORDS, then judge
            // acceptance with the per-package accepted keywords. The structured
            // visibility keeps a hard-mask reason distinct from a keyword reason;
            // the resolver only needs the accept/reject decision here.
            let keywords = self.config.stacked_keywords(&pref, &ebuild_keywords);
            let extra = self.config.package_keywords(&pref);
            if !matches!(
                self.config.visibility(&pref, &keywords, &extra),
                moraine_config::Visibility::Visible
            ) {
                return false;
            }
            // License acceptance: reduce LICENSE against the resolved USE and
            // mask the package when any required license is not accepted.
            if !meta.license.is_empty() {
                let iuse: Vec<String> = entry
                    .iuse
                    .iter()
                    .filter_map(|s| interner.resolve(*s).map(|x| x.to_string()))
                    .collect();
                let restrict_test = entry
                    .restrict
                    .iter()
                    .any(|r| interner.resolve(*r).as_deref() == Some("test"));
                let use_set = self
                    .config
                    .effective_use(&pref, &iuse, self.stable, restrict_test)
                    .enabled;
                let reduced = crate::license::reduce_license(&meta.license, &use_set);
                if !self.config.missing_licenses(&reduced, &pref).is_empty() {
                    return false;
                }
            }
            return true;
        }
        false
    }

    fn has_binary(&self, cp: &str, version: &Version) -> bool {
        binary_offered(
            &self.binaries,
            self.binary_target.as_ref(),
            &format!("{cp}-{version}"),
        )
    }

    fn acceptability(&self, meta: &PackageMeta) -> Acceptability {
        let Some((category, package)) = Self::split_cp(&meta.cp) else {
            return Acceptability::HardMasked;
        };
        for cand in self.repo.match_atom_str(&meta.cp) {
            let store = &self.repo.repos()[cand.repo_order].store;
            let interner = store.interner();
            let entry = cand.entry;
            if entry.version != meta.version {
                continue;
            }
            let pref = PackageRef {
                category: interner.intern(category),
                package: interner.intern(package),
                version: &entry.version,
                slot: Some(entry.slot),
                subslot: entry.subslot,
                repo: Some(entry.repository),
            };
            let ebuild_keywords: Vec<String> = entry
                .keywords
                .iter()
                .filter_map(|k| interner.resolve(*k).map(|x| x.to_string()))
                .collect();
            let keywords = self.config.stacked_keywords(&pref, &ebuild_keywords);
            let extra = self.config.package_keywords(&pref);
            let mut change = AcceptChange::default();
            match self.config.visibility(&pref, &keywords, &extra) {
                moraine_config::Visibility::Visible => {}
                moraine_config::Visibility::HardMasked(_) => return Acceptability::HardMasked,
                moraine_config::Visibility::NeedsKeyword => {
                    change.keyword = Some(format!("~{}", self.config.arch));
                }
                moraine_config::Visibility::NeedsDoubleStar => {
                    change.keyword = Some("**".to_owned());
                }
            }
            if !meta.license.is_empty() {
                let iuse: Vec<String> = entry
                    .iuse
                    .iter()
                    .filter_map(|s| interner.resolve(*s).map(|x| x.to_string()))
                    .collect();
                let restrict_test = entry
                    .restrict
                    .iter()
                    .any(|r| interner.resolve(*r).as_deref() == Some("test"));
                let use_set = self
                    .config
                    .effective_use(&pref, &iuse, self.stable, restrict_test)
                    .enabled;
                let reduced = crate::license::reduce_license(&meta.license, &use_set);
                let missing = self.config.missing_licenses(&reduced, &pref);
                if !missing.is_empty() {
                    change.licenses = missing.into_iter().collect();
                }
            }
            return if change.is_empty() {
                Acceptability::Visible
            } else {
                Acceptability::NeedsAccept(change)
            };
        }
        Acceptability::HardMasked
    }

    fn resolved_use(&self, meta: &PackageMeta) -> BTreeSet<String> {
        let (category, package) = match Self::split_cp(&meta.cp) {
            Some(p) => p,
            None => return BTreeSet::new(),
        };
        for cand in self.repo.match_atom_str(&meta.cp) {
            let store = &self.repo.repos()[cand.repo_order].store;
            let interner = store.interner();
            let entry = cand.entry;
            if entry.version != meta.version {
                continue;
            }
            let pref = PackageRef {
                category: interner.intern(category),
                package: interner.intern(package),
                version: &entry.version,
                slot: Some(entry.slot),
                subslot: entry.subslot,
                repo: Some(entry.repository),
            };
            // Pass the raw IUSE (with `+`/`-` prefixes) so defaults apply.
            let iuse: Vec<String> = entry
                .iuse
                .iter()
                .filter_map(|s| interner.resolve(*s).map(|x| x.to_string()))
                .collect();
            // FEATURES=test injects `test`, re-disabled when RESTRICT contains
            // `test`, so the test phase and the USE share one source of truth.
            let restrict_test = entry
                .restrict
                .iter()
                .any(|r| interner.resolve(*r).as_deref() == Some("test"));
            return self
                .config
                .effective_use(&pref, &iuse, self.stable, restrict_test)
                .enabled;
        }
        BTreeSet::new()
    }

    fn forced_use(&self, meta: &PackageMeta) -> BTreeSet<String> {
        let (category, package) = match Self::split_cp(&meta.cp) {
            Some(p) => p,
            None => return BTreeSet::new(),
        };
        for cand in self.repo.match_atom_str(&meta.cp) {
            let store = &self.repo.repos()[cand.repo_order].store;
            let interner = store.interner();
            let entry = cand.entry;
            if entry.version != meta.version {
                continue;
            }
            let pref = PackageRef {
                category: interner.intern(category),
                package: interner.intern(package),
                version: &entry.version,
                slot: Some(entry.slot),
                subslot: entry.subslot,
                repo: Some(entry.repository),
            };
            let iuse: Vec<String> = entry
                .iuse
                .iter()
                .filter_map(|s| interner.resolve(*s).map(|x| x.to_string()))
                .collect();
            let restrict_test = entry
                .restrict
                .iter()
                .any(|r| interner.resolve(*r).as_deref() == Some("test"));
            // `forced` is the union of `use.force` and `use.mask`: exactly the
            // flags whose state the profile fixes, which the `--newuse` IUSE
            // difference subtracts before triggering a reinstall.
            return self
                .config
                .effective_use(&pref, &iuse, self.stable, restrict_test)
                .forced;
        }
        BTreeSet::new()
    }

    fn locked_use(&self, meta: &PackageMeta) -> BTreeSet<String> {
        let (category, package) = match Self::split_cp(&meta.cp) {
            Some(p) => p,
            None => return BTreeSet::new(),
        };
        for cand in self.repo.match_atom_str(&meta.cp) {
            let store = &self.repo.repos()[cand.repo_order].store;
            let interner = store.interner();
            let entry = cand.entry;
            if entry.version != meta.version {
                continue;
            }
            let pref = PackageRef {
                category: interner.intern(category),
                package: interner.intern(package),
                version: &entry.version,
                slot: Some(entry.slot),
                subslot: entry.subslot,
                repo: Some(entry.repository),
            };
            let iuse: Vec<String> = entry
                .iuse
                .iter()
                .filter_map(|s| interner.resolve(*s).map(|x| x.to_string()))
                .collect();
            let restrict_test = entry
                .restrict
                .iter()
                .any(|r| interner.resolve(*r).as_deref() == Some("test"));
            // `forced` is the union of `use.force` and `use.mask`: exactly the
            // flags whose state the user cannot change, which autounmask must not
            // propose to toggle.
            return self
                .config
                .effective_use(&pref, &iuse, self.stable, restrict_test)
                .forced;
        }
        BTreeSet::new()
    }

    fn is_provided(&self, cp: &str, version: &Version) -> bool {
        let (category, package) = match Self::split_cp(cp) {
            Some(p) => p,
            None => return false,
        };
        for cand in self.repo.match_atom_str(cp) {
            let store = &self.repo.repos()[cand.repo_order].store;
            let interner = store.interner();
            let entry = cand.entry;
            if &entry.version != version {
                continue;
            }
            let pref = PackageRef {
                category: interner.intern(category),
                package: interner.intern(package),
                version,
                slot: Some(entry.slot),
                subslot: entry.subslot,
                repo: Some(entry.repository),
            };
            return self.config.is_provided(&pref);
        }
        false
    }

    fn installed(&self, cp: &str) -> Vec<InstalledMeta> {
        let (category, package) = match Self::split_cp(cp) {
            Some(p) => p,
            None => return Vec::new(),
        };
        let interner = self.vdb.interner();
        let cat_sym = interner.intern(category);
        let pkg_sym = interner.intern(package);
        self.vdb
            .records()
            .iter()
            .filter(|record| record.category == cat_sym && record.package == pkg_sym)
            .map(|record| self.record_to_installed(cp.to_owned(), record))
            .collect()
    }

    fn installed_all(&self) -> Vec<InstalledMeta> {
        let interner = self.vdb.interner();
        self.vdb
            .records()
            .iter()
            .filter_map(|record| {
                let category = interner.resolve(record.category)?;
                let package = interner.resolve(record.package)?;
                let cp = format!("{category}/{package}");
                Some(self.record_to_installed(cp, record))
            })
            .collect()
    }

    fn libc_providers(&self) -> BTreeSet<String> {
        // Expand `virtual/libc`'s RDEPEND providers from the repository, then
        // keep only those actually installed, mirroring `find_libc_deps` running
        // `expand_new_virt` over the vartree.
        let mut out = BTreeSet::new();
        for vmeta in self.versions_of(LIBC_PACKAGE_ATOM) {
            collect_leaf_cps(&vmeta.rdepend, &mut out);
        }
        out.retain(|cp| !self.installed(cp).is_empty());
        out
    }
}

/// Collect the `category/package` of every non-blocker leaf atom in a dependency
/// node, ignoring USE conditionals, so the libc-provider scan sees every concrete
/// provider a new-style virtual could pull in.
fn collect_leaf_cps(node: &DepNode, out: &mut BTreeSet<String>) {
    match node {
        DepNode::Leaf(atom) => {
            if atom.blocker == BlockerKind::None {
                out.insert(atom.cp.clone());
            }
        }
        DepNode::AllOf(children)
        | DepNode::AnyOf(children)
        | DepNode::ExactlyOneOf(children)
        | DepNode::AtMostOneOf(children) => {
            for c in children {
                collect_leaf_cps(c, out);
            }
        }
        DepNode::Conditional { body, .. } => {
            for c in body {
                collect_leaf_cps(c, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moraine_binpkg::MetadataMap;
    use moraine_binpkg::metadata::{KEY_CHOST, KEY_USE};

    fn candidate(use_str: &str, chost: &str) -> BinaryCandidate {
        let mut m = MetadataMap::new();
        m.set_str(KEY_USE, use_str);
        m.set_str(KEY_CHOST, chost);
        BinaryCandidate {
            cp: "dev-libs/foo".into(),
            version: Version::parse("1").unwrap(),
            metadata: m,
        }
    }

    fn target(use_flags: &[&str], chost: &str) -> TargetConfig {
        TargetConfig {
            chost: chost.into(),
            selected_use: use_flags.iter().map(|s| s.to_string()).collect(),
            forced_use: BTreeSet::new(),
            masked_use: BTreeSet::new(),
            available_sonames: BTreeSet::new(),
        }
    }

    #[test]
    fn incompatible_binary_is_not_offered() {
        let target = target(&["ssl"], "x86_64-pc-linux-gnu");
        let mut binaries = HashMap::new();
        binaries.insert(
            "dev-libs/foo-1".to_string(),
            candidate("ssl", "x86_64-pc-linux-gnu"),
        );
        // A foreign CHOST is not offered.
        binaries.insert(
            "dev-libs/bar-1".to_string(),
            candidate("ssl", "i686-pc-linux-gnu"),
        );
        // A mismatched USE is not offered.
        binaries.insert(
            "dev-libs/baz-1".to_string(),
            candidate("", "x86_64-pc-linux-gnu"),
        );

        assert!(binary_offered(&binaries, Some(&target), "dev-libs/foo-1"));
        assert!(!binary_offered(&binaries, Some(&target), "dev-libs/bar-1"));
        assert!(!binary_offered(&binaries, Some(&target), "dev-libs/baz-1"));
        // Absent candidate is never offered.
        assert!(!binary_offered(&binaries, Some(&target), "dev-libs/none-1"));
        // With no target, a present candidate is offered on presence alone.
        assert!(binary_offered(&binaries, None, "dev-libs/bar-1"));
    }
}
