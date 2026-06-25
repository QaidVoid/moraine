//! The data source the Gentoo provider reads from.
//!
//! This boundary lets the provider draw candidates, metadata, visibility, USE,
//! and installed state from the real `moraine-repo`, `moraine-vdb`, and
//! `moraine-config` crates, or from in-memory test doubles built directly. All
//! identifiers cross the boundary as owned strings so that the resolver keeps a
//! single canonical namespace and never mixes symbols from different interners.

use std::collections::BTreeSet;

use moraine_version::Version;

use crate::depnode::DepNode;

/// Repository metadata for one concrete package version, with normalized
/// dependency ASTs and the data needed for visibility and USE resolution.
#[derive(Debug, Clone)]
pub struct PackageMeta {
    /// The `category/package`.
    pub cp: String,
    /// The version.
    pub version: Version,
    /// The EAPI string.
    pub eapi: String,
    /// The slot.
    pub slot: String,
    /// The sub-slot, if any.
    pub subslot: Option<String>,
    /// `DEPEND` AST.
    pub depend: DepNode,
    /// `BDEPEND` AST.
    pub bdepend: DepNode,
    /// `RDEPEND` AST.
    pub rdepend: DepNode,
    /// `PDEPEND` AST.
    pub pdepend: DepNode,
    /// `IDEPEND` AST.
    pub idepend: DepNode,
    /// `REQUIRED_USE` AST.
    pub required_use: DepNode,
    /// Raw `LICENSE` text (a USE-conditional dep-string of license tokens).
    pub license: String,
    /// The declared IUSE flags (without `+`/`-` prefixes).
    pub iuse: BTreeSet<String>,
}

/// An installed package, with its recorded slot, sub-slot, USE, and `:=`
/// bindings.
#[derive(Debug, Clone)]
pub struct InstalledMeta {
    /// The `category/package`.
    pub cp: String,
    /// The installed version.
    pub version: Version,
    /// The recorded slot.
    pub slot: String,
    /// The recorded sub-slot, if any.
    pub subslot: Option<String>,
    /// The recorded enabled USE flags.
    pub use_enabled: BTreeSet<String>,
    /// The declared IUSE flags (bare names, no `+`/`-`), needed so a USE-dep
    /// default (`[flag(+)]`) is not applied to a flag the package actually
    /// declares but has disabled.
    pub iuse: BTreeSet<String>,
    /// Recorded `:=`/`:slot=` bindings: `(dependency_cp, slot, subslot)`.
    pub slot_bindings: Vec<(String, String, Option<String>)>,
}

/// The data the Gentoo provider needs about packages, configuration, and the
/// installed store.
pub trait ResolveSource {
    /// All known versions of `cp` from the repository, in ascending version
    /// order.
    fn versions_of(&self, cp: &str) -> Vec<PackageMeta>;

    /// Whether the given package version is visible (passes package.mask and
    /// keyword acceptance). USE masking is reflected through `resolved_use`.
    fn is_visible(&self, meta: &PackageMeta) -> bool;

    /// The resolved enabled USE flags for the given package version.
    fn resolved_use(&self, meta: &PackageMeta) -> BTreeSet<String>;

    /// Whether an atom of `cp` constrained to `version` is satisfied by a
    /// `package.provided` entry (so no install is needed).
    fn is_provided(&self, cp: &str, version: &Version) -> bool;

    /// The installed packages for `cp`, if any.
    fn installed(&self, cp: &str) -> Vec<InstalledMeta>;

    /// Whether any installed package satisfies `cp` at `version` (used to mark
    /// already-installed packages and satisfied edges).
    fn installed_matches(&self, cp: &str, version: &Version, slot: &str) -> bool {
        self.installed(cp)
            .iter()
            .any(|i| &i.version == version && i.slot == slot)
    }
}
