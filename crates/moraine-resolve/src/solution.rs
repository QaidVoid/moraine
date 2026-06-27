//! The resolved-solution output: the stable contract consumed by the
//! merge-order phase and downstream build and merge engines.

use std::collections::BTreeSet;

use moraine_version::Version;

/// A Gentoo dependency class. The class determines the target root and whether
/// the edge is build-time or runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepClass {
    /// `BDEPEND`: build-host build-time dependency.
    Bdepend,
    /// `DEPEND`: target/host build-time dependency.
    Depend,
    /// `RDEPEND`: runtime dependency.
    Rdepend,
    /// `PDEPEND`: post-merge runtime dependency.
    Pdepend,
    /// `IDEPEND`: install-time dependency (within the runtime tier).
    Idepend,
}

impl DepClass {
    /// Whether this class is a build-time dependency class.
    pub fn is_build_time(self) -> bool {
        matches!(self, DepClass::Bdepend | DepClass::Depend)
    }

    /// Whether this class is a runtime dependency class.
    pub fn is_runtime(self) -> bool {
        !self.is_build_time()
    }

    /// The human-readable name of the class.
    pub fn name(self) -> &'static str {
        match self {
            DepClass::Bdepend => "BDEPEND",
            DepClass::Depend => "DEPEND",
            DepClass::Rdepend => "RDEPEND",
            DepClass::Pdepend => "PDEPEND",
            DepClass::Idepend => "IDEPEND",
        }
    }
}

/// A target installation root for a dependency edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Root {
    /// The running build host (BROOT). `BDEPEND` resolves here, and `DEPEND`
    /// resolves here when the EAPI lacks bdepend support.
    BuildHost,
    /// The target/host sysroot (ESYSROOT). `DEPEND` resolves here when the EAPI
    /// provides bdepend.
    TargetSysroot,
    /// The target root (ROOT). Runtime classes resolve here.
    Target,
}

/// A recorded slot/sub-slot binding from a `:=` or `:slot=` dependency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotBinding {
    /// The `category/package` of the depended-upon package.
    pub dependency: String,
    /// The bound slot.
    pub slot: String,
    /// The bound sub-slot, if any.
    pub subslot: Option<String>,
    /// The root against which the binding was resolved.
    pub root: Root,
}

/// A specific installed package an actionable blocker removes: the exact
/// `(cp, version, slot)` matched by the blocker's atom, so an uninstall touches
/// only the matching entries rather than every version and slot of the cp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockVictim {
    /// The `category/package`.
    pub cp: String,
    /// The exact installed version to remove.
    pub version: Version,
    /// The exact installed slot to remove.
    pub slot: String,
}

/// A blocker recorded in the solution for the merge-order phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedBlocker {
    /// The `category/package` of the blocking package.
    pub blocker: String,
    /// The blocked atom, rendered.
    pub blocked_atom: String,
    /// Whether the blocker is strong (`!!`).
    pub strong: bool,
    /// The exact installed entries this blocker removes (atom-filtered by version
    /// and slot). Empty when the blocker is informational and removes nothing.
    pub victims: Vec<BlockVictim>,
}

/// A class-tagged dependency edge between two packages in the solution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepEdge {
    /// The dependent package, as a slot-qualified `category/package:slot` key, so
    /// an edge from a `cp` co-installed in two slots names the specific slot the
    /// dependency was declared by. Use [`endpoint_cp`] to recover the bare `cp`.
    pub from: String,
    /// The depended-upon package, as a slot-qualified `category/package:slot` key,
    /// so an edge into a `cp` co-installed in two slots targets the specific slot
    /// the dependency resolved against. Use [`endpoint_cp`] to recover the `cp`.
    pub to: String,
    /// The dependency class of the edge.
    pub class: DepClass,
    /// The target root the dependency resolves against.
    pub root: Root,
    /// Whether the edge is build-time (else runtime).
    pub build_time: bool,
    /// Whether the dependency used a slot operator (`:=`, `:slot=`).
    pub slot_op: bool,
    /// Whether the edge came from an optional `||` branch the solution did not
    /// strictly require.
    pub optional: bool,
}

/// A package selected by resolution, with its chosen version, USE, and slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPackage {
    /// The `category/package`.
    pub cp: String,
    /// The chosen version.
    pub version: Version,
    /// The chosen slot.
    pub slot: String,
    /// The chosen sub-slot, if any.
    pub subslot: Option<String>,
    /// The resolved USE flags enabled on this package.
    pub use_enabled: BTreeSet<String>,
    /// The `:=`/`:slot=` bindings this package recorded against its providers.
    pub slot_bindings: Vec<SlotBinding>,
    /// Whether this package is already installed at the same version (a no-op
    /// reinstall candidate) versus a new install or upgrade.
    pub already_installed: bool,
    /// Whether this package must be rebuilt because a `:=`/`:slot=` provider's
    /// sub-slot changed relative to its recorded binding.
    pub subslot_rebuild: bool,
}

impl ResolvedPackage {
    /// The `category/package-version` string for this package.
    pub fn cpv(&self) -> String {
        format!("{}-{}", self.cp, self.version)
    }
}

/// A configuration change autounmask must report for a selected package that is
/// only installable after accepting a keyword or license.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutounmaskChange {
    /// The `category/package`.
    pub cp: String,
    /// The exact version that needs the change.
    pub version: Version,
    /// The keyword/license acceptance required.
    pub change: crate::source::AcceptChange,
}

/// The full output of a successful resolution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedSolution {
    /// The packages to install, keyed and ordered by `category/package`.
    pub packages: Vec<ResolvedPackage>,
    /// The class-tagged dependency edges between selected packages.
    pub edges: Vec<DepEdge>,
    /// Blockers recorded for the merge-order phase.
    pub blockers: Vec<RecordedBlocker>,
    /// The number of conflict-driven backjumps the solver performed.
    pub backtracks: u32,
    /// Keyword/license acceptance changes required by newly-merged packages.
    pub autounmask: Vec<AutounmaskChange>,
}

impl ResolvedSolution {
    /// Look up a resolved package by its `category/package`, returning the first
    /// matching slot. Use this for `cp`-level membership checks (for example,
    /// whether any slot of a `cp` is being merged); use [`Self::package_slot`] or
    /// [`Self::package_by_key`] when the specific slot matters.
    pub fn package(&self, cp: &str) -> Option<&ResolvedPackage> {
        self.packages.iter().find(|p| p.cp == cp)
    }

    /// Look up a resolved package by its exact `(category/package, slot)`.
    pub fn package_slot(&self, cp: &str, slot: &str) -> Option<&ResolvedPackage> {
        self.packages.iter().find(|p| p.cp == cp && p.slot == slot)
    }

    /// Look up a resolved package by a slot-qualified `category/package:slot` key.
    /// A bare `cp` key (no slot separator) falls back to the first matching slot,
    /// so hand-built solutions whose edges name bare `cp`s still resolve.
    pub fn package_by_key(&self, key: &str) -> Option<&ResolvedPackage> {
        match key.split_once(':') {
            Some((cp, slot)) => self.package_slot(cp, slot),
            None => self.package(key),
        }
    }
}

/// Recover the bare `category/package` from a slot-qualified `cp:slot` edge
/// endpoint or node key. A key without a slot separator is returned unchanged.
pub fn endpoint_cp(key: &str) -> &str {
    key.split_once(':').map(|(cp, _)| cp).unwrap_or(key)
}
