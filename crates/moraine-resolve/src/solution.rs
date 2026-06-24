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

/// A blocker recorded in the solution for the merge-order phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedBlocker {
    /// The `category/package` of the blocking package.
    pub blocker: String,
    /// The blocked atom, rendered.
    pub blocked_atom: String,
    /// Whether the blocker is strong (`!!`).
    pub strong: bool,
}

/// A class-tagged dependency edge between two packages in the solution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepEdge {
    /// The dependent package, as `category/package`.
    pub from: String,
    /// The depended-upon package, as `category/package`.
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

/// The full output of a successful resolution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedSolution {
    /// The packages to install, keyed and ordered by `category/package`.
    pub packages: Vec<ResolvedPackage>,
    /// The class-tagged dependency edges between selected packages.
    pub edges: Vec<DepEdge>,
    /// Blockers recorded for the merge-order phase.
    pub blockers: Vec<RecordedBlocker>,
}

impl ResolvedSolution {
    /// Look up a resolved package by its `category/package`.
    pub fn package(&self, cp: &str) -> Option<&ResolvedPackage> {
        self.packages.iter().find(|p| p.cp == cp)
    }
}
