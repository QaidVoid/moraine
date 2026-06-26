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
    /// The recorded `*DEPEND` strings keyed by family (`DEPEND`, `RDEPEND`, ...),
    /// used by `--changed-deps` to compare against the current ebuild. Empty for
    /// sources that do not track recorded dependencies.
    pub recorded_deps: std::collections::BTreeMap<String, String>,
}

/// The configuration change needed to make a soft-masked package installable,
/// for autounmask reporting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptChange {
    /// The keyword to accept (for example `~amd64` or `**`), if keyword-masked.
    pub keyword: Option<String>,
    /// The licenses that must be accepted, if license-masked.
    pub licenses: Vec<String>,
}

impl AcceptChange {
    /// Whether this change carries no required acceptance.
    pub fn is_empty(&self) -> bool {
        self.keyword.is_none() && self.licenses.is_empty()
    }
}

/// A package version's installability, distinguishing autounmaskable soft masks
/// (keyword/license) from hard masks (`package.mask`), which autounmask leaves
/// alone by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acceptability {
    /// Installable with no configuration change.
    Visible,
    /// Installable only after accepting the given keyword/license change.
    NeedsAccept(AcceptChange),
    /// Blocked by a hard mask; not autounmasked.
    HardMasked,
}

impl Acceptability {
    /// Whether autounmask may pull this version in. Like Portage's default, a
    /// `**` keyword (broken/empty KEYWORDS, typically a live ebuild) is not
    /// autounmasked; only `~arch` keywords and licenses are.
    pub fn is_autounmaskable(&self) -> bool {
        matches!(self, Acceptability::NeedsAccept(c) if c.keyword.as_deref() != Some("**"))
    }
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

    /// Whether a binary package is available for `cp` at `version` (local or from
    /// the binhost). The default is `false`; `RealSource` consults the binhost so
    /// version selection can prefer a binary over a higher source-only version
    /// under `getbinpkg`, like Portage.
    fn has_binary(&self, _cp: &str, _version: &Version) -> bool {
        false
    }

    /// Classify a version for autounmask: visible, soft-masked (keyword/license,
    /// reportable as a change), or hard-masked. The default treats any invisible
    /// package as hard-masked; `RealSource` distinguishes the soft cases.
    fn acceptability(&self, meta: &PackageMeta) -> Acceptability {
        if self.is_visible(meta) {
            Acceptability::Visible
        } else {
            Acceptability::HardMasked
        }
    }

    /// The resolved enabled USE flags for the given package version.
    fn resolved_use(&self, meta: &PackageMeta) -> BTreeSet<String>;

    /// Whether an atom of `cp` constrained to `version` is satisfied by a
    /// `package.provided` entry (so no install is needed).
    fn is_provided(&self, cp: &str, version: &Version) -> bool;

    /// The installed packages for `cp`, if any.
    fn installed(&self, cp: &str) -> Vec<InstalledMeta>;

    /// Every installed package across all `cp`s. Used by the installed-package
    /// blocker scan and the slot-operator reverse-dependency pull-in. The default
    /// is empty for sources that do not expose a full installed store.
    fn installed_all(&self) -> Vec<InstalledMeta> {
        Vec::new()
    }

    /// Whether any installed package satisfies `cp` at `version` (used to mark
    /// already-installed packages and satisfied edges).
    fn installed_matches(&self, cp: &str, version: &Version, slot: &str) -> bool {
        self.installed(cp)
            .iter()
            .any(|i| &i.version == version && i.slot == slot)
    }
}
