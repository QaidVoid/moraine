//! The per-package record held in memory after a load.
//!
//! A [`PackageRecord`] carries the resolution-relevant aux keys, the soname
//! linkage, and a reference to the saved build environment. The `*DEPEND` fields
//! keep both the original recorded string and the parsed
//! [`moraine_atom::DepSpec`] AST. The original string is the source of truth for
//! round-trip fidelity, including any `:=` slot or sub-slot binding written
//! verbatim at build time; the AST is materialized at load so the resolver
//! consumes structure without reparsing.

use moraine_atom::DepSpec;
use moraine_common::{Interner, Symbol};
use moraine_eapi::EapiFeatures;
use moraine_version::Version;

use crate::contents::Contents;
use crate::soname::{Provides, Requires};

/// A recorded slot, keeping the slot and any sub-slot exactly as stored.
///
/// Slot and sub-slot are preserved without normalization so slot-operator
/// rebuild detection downstream can compare recorded sub-slots faithfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// The interned slot name.
    pub slot: Symbol,
    /// The interned sub-slot name, if the recorded `SLOT` carried one.
    pub subslot: Option<Symbol>,
}

/// Which `*DEPEND` family a recorded dependency string belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependKind {
    /// `DEPEND`: build-time dependencies on the target.
    Depend,
    /// `RDEPEND`: runtime dependencies.
    RDepend,
    /// `BDEPEND`: build-host dependencies (meaningful at EAPI 7+).
    BDepend,
    /// `PDEPEND`: post-merge dependencies.
    PDepend,
    /// `IDEPEND`: install-time dependencies (meaningful at EAPI 8+).
    IDepend,
}

impl DependKind {
    /// The canonical uppercase field name.
    pub const fn name(self) -> &'static str {
        match self {
            DependKind::Depend => "DEPEND",
            DependKind::RDepend => "RDEPEND",
            DependKind::BDepend => "BDEPEND",
            DependKind::PDepend => "PDEPEND",
            DependKind::IDepend => "IDEPEND",
        }
    }

    /// The five dependency kinds in a stable order.
    pub const ALL: [DependKind; 5] = [
        DependKind::Depend,
        DependKind::RDepend,
        DependKind::BDepend,
        DependKind::PDepend,
        DependKind::IDepend,
    ];

    /// Whether this dependency kind is meaningful under the given EAPI features.
    ///
    /// `BDEPEND` is only meaningful at EAPI 7+ and `IDEPEND` at EAPI 8+; the rest
    /// are meaningful at every EAPI.
    pub const fn is_meaningful(self, features: EapiFeatures) -> bool {
        match self {
            DependKind::BDepend => features.bdepend,
            DependKind::IDepend => features.idepend,
            _ => true,
        }
    }
}

/// A recorded `*DEPEND` field: the original string plus its parsed AST.
#[derive(Debug, Clone)]
pub struct Depend {
    /// The original recorded dependency string, preserved verbatim including any
    /// `:=` slot/sub-slot binding written at build time.
    pub raw: String,
    /// The parsed dependency AST, materialized at load.
    pub ast: DepSpec,
}

/// An installed package, fully materialized in memory.
#[derive(Debug, Clone)]
pub struct PackageRecord {
    /// The interned category.
    pub category: Symbol,
    /// The interned package name.
    pub package: Symbol,
    /// The package version.
    pub version: Version,
    /// The recorded EAPI string.
    pub eapi: String,
    /// The recorded slot and sub-slot.
    pub slot: Slot,
    /// The recorded USE flags, interned.
    pub use_flags: Vec<Symbol>,
    /// The recorded IUSE tokens (kept as strings; default markers preserved).
    pub iuse: Vec<String>,
    /// The recorded `*DEPEND` fields, indexed by [`DependKind`].
    pub depends: DependSet,
    /// The recorded `KEYWORDS`.
    pub keywords: Vec<String>,
    /// The recorded `LICENSE` string.
    pub license: String,
    /// The recorded `PROPERTIES` string.
    pub properties: String,
    /// The recorded `RESTRICT` string.
    pub restrict: String,
    /// The interned origin repository, if recorded.
    pub repository: Option<Symbol>,
    /// The recorded `DEFINED_PHASES`.
    pub defined_phases: Vec<String>,
    /// The recorded `BUILD_TIME`, if present.
    pub build_time: Option<u64>,
    /// The recorded `BUILD_ID`, if present.
    pub build_id: Option<u64>,
    /// The per-package counter value in effect at write time.
    pub counter: u64,
    /// The recorded `CHOST`.
    pub chost: String,
    /// The soname linkage this package provides.
    pub provides: Provides,
    /// The soname linkage this package requires.
    pub requires: Requires,
    /// The CONTENTS manifest of installed files.
    pub contents: Contents,
    /// A reference to the saved build environment blob, if one was recorded.
    pub environment: Option<EnvironmentRef>,
}

/// The five `*DEPEND` fields kept together, addressable by [`DependKind`].
#[derive(Debug, Clone, Default)]
pub struct DependSet {
    /// `DEPEND`.
    pub depend: Option<Depend>,
    /// `RDEPEND`.
    pub rdepend: Option<Depend>,
    /// `BDEPEND`.
    pub bdepend: Option<Depend>,
    /// `PDEPEND`.
    pub pdepend: Option<Depend>,
    /// `IDEPEND`.
    pub idepend: Option<Depend>,
}

impl DependSet {
    /// Borrow the recorded dependency for `kind`, if present.
    pub fn get(&self, kind: DependKind) -> Option<&Depend> {
        match kind {
            DependKind::Depend => self.depend.as_ref(),
            DependKind::RDepend => self.rdepend.as_ref(),
            DependKind::BDepend => self.bdepend.as_ref(),
            DependKind::PDepend => self.pdepend.as_ref(),
            DependKind::IDepend => self.idepend.as_ref(),
        }
    }

    /// Mutable slot for `kind`, used by the loader and importer.
    pub fn slot_mut(&mut self, kind: DependKind) -> &mut Option<Depend> {
        match kind {
            DependKind::Depend => &mut self.depend,
            DependKind::RDepend => &mut self.rdepend,
            DependKind::BDepend => &mut self.bdepend,
            DependKind::PDepend => &mut self.pdepend,
            DependKind::IDepend => &mut self.idepend,
        }
    }

    /// Borrow the dependency for `kind` only when it is meaningful under
    /// `features`. Returns `None` for `BDEPEND`/`IDEPEND` on EAPIs that predate
    /// them even if a string happens to be stored.
    pub fn get_meaningful(&self, kind: DependKind, features: EapiFeatures) -> Option<&Depend> {
        if kind.is_meaningful(features) {
            self.get(kind)
        } else {
            None
        }
    }
}

/// A reference to a saved build-environment blob.
///
/// The blob itself is content-addressed by its BLAKE3 digest so identical
/// environments deduplicate. The resolver never reads it; merge phases fetch it
/// by digest. The raw bytes are carried alongside the digest in memory so the
/// importer can hand them to a blob store later without re-reading the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentRef {
    /// The BLAKE3 digest of the compressed environment blob, lowercase hex.
    pub digest: String,
    /// The compressed environment bytes (`environment.bz2` contents).
    pub blob: Vec<u8>,
}

impl PackageRecord {
    /// The EAPI feature set for this record's recorded EAPI.
    pub fn features(&self) -> EapiFeatures {
        moraine_eapi::features_for(&self.eapi)
    }

    /// Render `category/package-version` using `interner` for the cp tokens.
    pub fn cpv(&self, interner: &Interner) -> String {
        let cat = interner.resolve(self.category);
        let pkg = interner.resolve(self.package);
        match (cat, pkg) {
            (Some(c), Some(p)) => format!("{c}/{p}-{}", self.version.as_str()),
            _ => format!("?/?-{}", self.version.as_str()),
        }
    }
}
