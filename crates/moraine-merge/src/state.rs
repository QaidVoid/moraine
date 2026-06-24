//! Installed-state recording and post-merge actions.
//!
//! The installed record carries the recorded USE, the resolved slot and sub-slot
//! including any `:=` bindings, the dependency variables, the saved build
//! environment, the soname `PROVIDES`/`REQUIRES`, and a counter drawn from a
//! global monotonic counter. The record becomes visible only after CONTENTS and
//! all files are durable, which is the commit point. After completion the engine
//! triggers elog dispatch, news-item marking, and config-update notices as
//! recorded outcomes.

use std::collections::BTreeMap;

use moraine_common::Interner;
use moraine_vdb::contents::{Contents, Entry};
use moraine_vdb::record::{Depend, DependKind, DependSet, EnvironmentRef, PackageRecord, Slot};
use moraine_vdb::soname::{Provides, Requires, SonameEntry};
use moraine_version::Version;

use crate::error::MergeError;

/// A soname tagged with its ABI bucket, as plain strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Soname {
    /// The ABI bucket token (for example `x86_64`).
    pub bucket: String,
    /// The soname (for example `libfoo.so.1`).
    pub soname: String,
}

/// The installed state to record for a merged package, as plain data.
///
/// This mirrors [`PackageRecord`] but holds strings rather than interned
/// [`Symbol`](moraine_common::Symbol)s so the caller does not need a shared
/// interner; the engine interns into the store at commit. Slot and sub-slot,
/// including any `:=` binding, are preserved verbatim in the `*DEPEND` strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageState {
    /// The `category/package-version`, used as the operation label.
    pub cpv: String,
    /// The category.
    pub category: String,
    /// The package name.
    pub package: String,
    /// The version string.
    pub version: String,
    /// The recorded EAPI.
    pub eapi: String,
    /// The resolved slot.
    pub slot: String,
    /// The resolved sub-slot, if any.
    pub subslot: Option<String>,
    /// The recorded USE flags.
    pub use_flags: Vec<String>,
    /// The recorded IUSE tokens.
    pub iuse: Vec<String>,
    /// The `*DEPEND` strings keyed by family name (`DEPEND`, `RDEPEND`, ...),
    /// preserved verbatim including any `:=` binding.
    pub depends: BTreeMap<String, String>,
    /// The recorded `KEYWORDS`.
    pub keywords: Vec<String>,
    /// The recorded `LICENSE`.
    pub license: String,
    /// The recorded `PROPERTIES`.
    pub properties: String,
    /// The recorded `RESTRICT`.
    pub restrict: String,
    /// The origin repository, if recorded.
    pub repository: Option<String>,
    /// The recorded `DEFINED_PHASES`.
    pub defined_phases: Vec<String>,
    /// The recorded `BUILD_TIME`.
    pub build_time: Option<u64>,
    /// The recorded `CHOST`.
    pub chost: String,
    /// The sonames this package provides.
    pub provides: Vec<Soname>,
    /// The sonames this package requires.
    pub requires: Vec<Soname>,
    /// The saved build environment blob, if one was captured.
    pub environment: Option<Vec<u8>>,
}

impl PackageState {
    /// Build a [`PackageRecord`] against `interner`, attaching `contents` and
    /// stamping `counter`.
    ///
    /// `*DEPEND` strings are parsed into ASTs; an unparsable string is recorded
    /// with an empty AST rather than failing the merge, since the verbatim string
    /// remains the source of truth for round-trip fidelity.
    pub(crate) fn into_record(
        self,
        interner: &Interner,
        contents: Contents,
        counter: u64,
    ) -> Result<PackageRecord, MergeError> {
        let version = Version::parse(&self.version).map_err(|_| MergeError::Version {
            version: self.version.clone(),
            package: format!("{}/{}", self.category, self.package),
        })?;

        let features = moraine_eapi::features_for(&self.eapi);
        let mut depends = DependSet::default();
        for kind in DependKind::ALL {
            if let Some(raw) = self.depends.get(kind.name()) {
                let ast = moraine_atom::DepSpec::parse(raw, features, interner)
                    .unwrap_or_else(|_| moraine_atom::DepSpec::AllOf(Vec::new()));
                *depends.slot_mut(kind) = Some(Depend {
                    raw: raw.clone(),
                    ast,
                });
            }
        }

        let provides = Provides {
            entries: self
                .provides
                .iter()
                .map(|s| SonameEntry {
                    bucket: interner.intern(&s.bucket),
                    soname: interner.intern(&s.soname),
                })
                .collect(),
        };
        let requires = Requires {
            entries: self
                .requires
                .iter()
                .map(|s| SonameEntry {
                    bucket: interner.intern(&s.bucket),
                    soname: interner.intern(&s.soname),
                })
                .collect(),
        };

        let environment = self.environment.map(|blob| EnvironmentRef {
            digest: moraine_common::hash::blake3(&blob),
            blob,
        });

        Ok(PackageRecord {
            category: interner.intern(&self.category),
            package: interner.intern(&self.package),
            version,
            eapi: self.eapi,
            slot: Slot {
                slot: interner.intern(&self.slot),
                subslot: self.subslot.as_deref().map(|s| interner.intern(s)),
            },
            use_flags: self.use_flags.iter().map(|u| interner.intern(u)).collect(),
            iuse: self.iuse,
            depends,
            keywords: self.keywords,
            license: self.license,
            properties: self.properties,
            restrict: self.restrict,
            repository: self.repository.as_deref().map(|r| interner.intern(r)),
            defined_phases: self.defined_phases,
            build_time: self.build_time,
            build_id: None,
            counter,
            chost: self.chost,
            provides,
            requires,
            contents,
            environment,
        })
    }
}

/// The recorded outcome of post-merge actions for an operation.
///
/// These are modeled as recorded outcomes rather than live dispatch to external
/// services. The CLI renders them; the merge engine does not run phase functions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PostMergeReport {
    /// elog messages dispatched for the operation.
    pub elog: Vec<String>,
    /// News items marked relevant by the operation.
    pub news_marked: Vec<String>,
    /// Pending config updates: the `._cfgNNNN_` variant install paths created.
    pub config_updates: Vec<String>,
}

impl PostMergeReport {
    /// Whether any post-merge action was recorded.
    pub fn is_empty(&self) -> bool {
        self.elog.is_empty() && self.news_marked.is_empty() && self.config_updates.is_empty()
    }
}

/// Helper for building the explicit CONTENTS entries of a merged package.
pub(crate) fn contents_from(entries: Vec<Entry>) -> Contents {
    Contents::from_entries(entries)
}
