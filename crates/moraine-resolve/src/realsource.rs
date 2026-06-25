//! A [`ResolveSource`] backed by the real `moraine-repo`, `moraine-vdb`, and
//! `moraine-config` crates.
//!
//! Each backing store interns identifiers against its own interner, so this
//! adapter resolves every token to an owned string at the boundary. Candidate
//! metadata, visibility, USE, `package.provided`, and installed state all flow
//! through the [`ResolveSource`] trait so the provider and encoder never touch a
//! foreign interner.

use std::collections::BTreeSet;

use moraine_atom::PackageRef;
use moraine_config::ResolvedConfig;
use moraine_repo::RepoIndex;
use moraine_vdb::Store;
use moraine_version::Version;

use crate::normalize::normalize_depspec;
use crate::required_use::parse_required_use;
use crate::source::{InstalledMeta, PackageMeta, ResolveSource};

/// A resolution source over a repository index, installed store, and resolved
/// configuration.
pub struct RealSource<'a> {
    repo: &'a RepoIndex,
    vdb: &'a Store,
    config: &'a ResolvedConfig,
    /// Whether to evaluate keyword acceptance as stable.
    stable: bool,
}

impl<'a> RealSource<'a> {
    /// Create a real source over the given backing stores.
    pub fn new(repo: &'a RepoIndex, vdb: &'a Store, config: &'a ResolvedConfig) -> Self {
        RealSource {
            repo,
            vdb,
            config,
            stable: false,
        }
    }

    /// Set whether keyword acceptance is evaluated as stable.
    pub fn with_stable(mut self, stable: bool) -> Self {
        self.stable = stable;
        self
    }

    /// Split a `category/package` into its parts.
    fn split_cp(cp: &str) -> Option<(&str, &str)> {
        cp.split_once('/')
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
                let use_set = self.config.effective_use(&pref, &iuse, self.stable).enabled;
                let reduced = crate::license::reduce_license(&meta.license, &use_set);
                if !self.config.missing_licenses(&reduced, &pref).is_empty() {
                    return false;
                }
            }
            return true;
        }
        false
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
            return self.config.effective_use(&pref, &iuse, self.stable).enabled;
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
        let mut out = Vec::new();
        for record in self.vdb.records() {
            if record.category != cat_sym || record.package != pkg_sym {
                continue;
            }
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
            out.push(InstalledMeta {
                cp: cp.to_owned(),
                version: record.version.clone(),
                slot,
                subslot,
                use_enabled,
                slot_bindings,
            });
        }
        out
    }
}
