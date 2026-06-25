//! The Gentoo dependency-provider for the generic solver.
//!
//! The solver's `Package` is an interned `category/package` string and its
//! `Version` is a [`moraine_version::Version`]. SLOT is treated as a candidate
//! filter and a post-resolution check; simultaneous installation of multiple
//! slots of the same `cp` is a known limitation deferred to a future revision,
//! so a slot collision (two required versions of one `cp`) surfaces naturally as
//! a solver conflict.
//!
//! `candidates` enumerates versions from the source, filters by the accumulated
//! range and by visibility, and ranks them in Portage `dep_zapdeps` order
//! (installed-slot match first, then upgrades), with a relaxed second pass that
//! includes masked-installed candidates when the strict pass is empty.
//!
//! `dependencies` reduces the version's USE-conditional groups against its
//! resolved USE, encodes the result into solver requirements, enforces
//! REQUIRED_USE on the chosen USE, and reports a sub-slot rebuild trigger when
//! the chosen provider's sub-slot differs from an installed `:=` binding.

use std::collections::BTreeSet;

use moraine_eapi::features_for;
use moraine_solver::{Dependencies, DependencyProvider, Range};
use moraine_version::Version;

use crate::depnode::DepNode;
use crate::encode::Encoder;
use crate::required_use::{RequiredUseOutcome, evaluate_required_use};
use crate::source::{PackageMeta, ResolveSource};

/// The synthetic root package name for a resolution request.
pub(crate) const REQUEST_CP: &str = "@request";

/// The solver package key for a `(cp, slot)`: `cp:slot`. A `cp`
/// (`category/package`) contains no colon and a main slot contains neither colon
/// nor slash, so the split is unambiguous and two slots of one `cp` are distinct
/// solver variables that can co-install.
pub(crate) fn package_key(cp: &str, slot: &str) -> String {
    format!("{cp}:{slot}")
}

/// Split a solver package key back into `(cp, slot)`. Returns `None` for the
/// synthetic root or any key without a slot separator.
pub(crate) fn split_key(key: &str) -> Option<(&str, &str)> {
    if key == REQUEST_CP {
        return None;
    }
    key.split_once(':')
}

/// The Gentoo dependency-provider.
pub struct GentooProvider<'s, S: ResolveSource> {
    source: &'s S,
    request: Vec<crate::depnode::NormAtom>,
}

impl<'s, S: ResolveSource> GentooProvider<'s, S> {
    /// Create a provider over the given data source with no request atoms (used
    /// when the provider is exercised directly in tests).
    pub fn new(source: &'s S) -> Self {
        GentooProvider {
            source,
            request: Vec::new(),
        }
    }

    /// Create a provider whose synthetic root depends on the given request
    /// atoms.
    pub(crate) fn with_request(source: &'s S, request: Vec<crate::depnode::NormAtom>) -> Self {
        GentooProvider { source, request }
    }

    /// Borrow the underlying source.
    pub fn source(&self) -> &'s S {
        self.source
    }

    /// The metadata for a concrete `(cp, slot)` at `version`, if known.
    pub(crate) fn meta(&self, cp: &str, slot: &str, version: &Version) -> Option<PackageMeta> {
        self.source
            .versions_of(cp)
            .into_iter()
            .find(|m| m.slot == slot && &m.version == version)
    }

    /// Rank the visible candidate versions of one `(cp, slot)` within `range`,
    /// highest version first. The slot is fixed by the solver key, so multiple
    /// slots of one `cp` are independent variables.
    fn ranked_candidates(&self, cp: &str, slot: &str, range: &Range<Version>) -> Vec<Version> {
        let installed = self.source.installed(cp);

        // Strict pass: only visible candidates of this slot.
        let mut strict: Vec<PackageMeta> = self
            .source
            .versions_of(cp)
            .into_iter()
            .filter(|m| m.slot == slot && range.contains(&m.version) && self.source.is_visible(m))
            .collect();

        if strict.is_empty() {
            // Relaxed pass: include masked-installed candidates of this slot.
            let mut relaxed: Vec<PackageMeta> = self
                .source
                .versions_of(cp)
                .into_iter()
                .filter(|m| {
                    m.slot == slot
                        && range.contains(&m.version)
                        && installed
                            .iter()
                            .any(|i| i.version == m.version && i.slot == m.slot)
                })
                .collect();
            relaxed.sort_by(|a, b| b.version.cmp(&a.version));
            return relaxed.into_iter().map(|m| m.version).collect();
        }

        // Prefer the version installed in this slot (Portage's default keeps an
        // installed package rather than needlessly upgrading or downgrading it),
        // then the highest version.
        let installed_versions: BTreeSet<Version> = installed
            .iter()
            .filter(|i| i.slot == slot)
            .map(|i| i.version.clone())
            .collect();
        strict.sort_by(|a, b| {
            let ai = installed_versions.contains(&a.version);
            let bi = installed_versions.contains(&b.version);
            bi.cmp(&ai).then_with(|| b.version.cmp(&a.version))
        });
        strict.into_iter().map(|m| m.version).collect()
    }
}

impl<S: ResolveSource> DependencyProvider for GentooProvider<'_, S> {
    type Package = String;
    type Version = Version;

    fn candidates(&self, package: &String, range: &Range<Version>) -> Vec<Version> {
        if package == REQUEST_CP {
            let v = Version::parse("0").expect("synthetic version parses");
            return if range.contains(&v) {
                vec![v]
            } else {
                Vec::new()
            };
        }
        match split_key(package) {
            Some((cp, slot)) => self.ranked_candidates(cp, slot, range),
            None => Vec::new(),
        }
    }

    fn dependencies(&self, package: &String, version: &Version) -> Dependencies<String, Version> {
        if package == REQUEST_CP {
            let encoder = Encoder {
                source: self.source,
            };
            return Dependencies::Known(encoder.request_requirements(&self.request));
        }
        let Some((cp, slot)) = split_key(package) else {
            return Dependencies::Unavailable(format!("malformed package key {package}"));
        };
        let Some(meta) = self.meta(cp, slot, version) else {
            return Dependencies::Unavailable(format!("no metadata for {package}-{version}"));
        };
        let package = cp;
        let features = features_for(&meta.eapi);
        let resolved_use = self.source.resolved_use(&meta);

        // Validate strong blockers against the EAPI feature table.
        let encoder = Encoder {
            source: self.source,
        };
        let nodes: [&DepNode; 5] = [
            &meta.bdepend,
            &meta.depend,
            &meta.rdepend,
            &meta.pdepend,
            &meta.idepend,
        ];
        if let Err(e) = encoder.validate_blockers(package, &nodes, features) {
            return Dependencies::Unavailable(e.to_string());
        }

        // Enforce REQUIRED_USE on the resolved USE; a violation makes this
        // (package, version, USE) choice unusable, which the solver records and
        // explains.
        if features.required_use
            && let RequiredUseOutcome::Violated(constraint) =
                evaluate_required_use(&meta.required_use, &resolved_use)
        {
            return Dependencies::Unavailable(format!("REQUIRED_USE violated: {constraint}"));
        }

        // An already-installed package at this exact version and slot is not
        // rebuilt, so its build-time dependencies are not pulled into the graph
        // (matching Portage's default and a binary install). A new install or
        // upgrade still pulls its build deps.
        let skip_build = self.source.installed_matches(cp, &meta.version, slot);

        match encoder.requirements(&meta, &resolved_use, features, skip_build) {
            Ok(reqs) => Dependencies::Known(reqs),
            Err(reason) => Dependencies::Unavailable(reason),
        }
    }
}
