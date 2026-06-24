//! The on-disk wire format.
//!
//! The store serializes strings and a string token table, never [`Symbol`]s or
//! parsed ASTs, because symbols are per-[`Interner`] and not stable across runs.
//! A fresh interner is built at load and the `*DEPEND` strings are parsed into
//! ASTs in memory.
//!
//! The primary file is a [`WireStore`]: a format version, the store counter, the
//! interned token table, and one [`WireRecord`] per package. Records reference
//! the token table by index for the repeated tokens (category, package, USE,
//! repository, soname buckets and sonames). The journal stores [`WireDelta`]
//! values, each self-contained with its own token table so it can be appended
//! without touching the primary file.
//!
//! [`Symbol`]: moraine_common::Symbol
//! [`Interner`]: moraine_common::Interner

use serde::{Deserialize, Serialize};

/// The format version this build reads and writes.
pub(crate) const FORMAT_VERSION: u32 = 1;

/// The complete primary store file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireStore {
    /// The on-disk format version.
    pub version: u32,
    /// The monotonic store counter at the time the file was written.
    pub counter: u64,
    /// The interned string token table; records index into this.
    pub tokens: Vec<String>,
    /// One entry per installed package.
    pub records: Vec<WireRecord>,
}

/// A single delta-journal entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum WireDelta {
    /// Add or replace a package record.
    Add {
        /// The delta's own token table.
        tokens: Vec<String>,
        /// The record being written.
        record: Box<WireRecord>,
    },
    /// Remove a package by category/package/version strings.
    Remove {
        /// The category.
        category: String,
        /// The package name.
        package: String,
        /// The version string.
        version: String,
        /// The counter value at which the removal was recorded.
        counter: u64,
    },
}

/// One package record on disk. Repeated tokens are token-table indices; the rest
/// are plain strings to keep the format self-describing and simple.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireRecord {
    /// Token index of the category.
    pub category: u32,
    /// Token index of the package name.
    pub package: u32,
    /// The version string.
    pub version: String,
    /// The EAPI string.
    pub eapi: String,
    /// Token index of the slot.
    pub slot: u32,
    /// Token index of the sub-slot, if any.
    pub subslot: Option<u32>,
    /// Token indices of the recorded USE flags.
    pub use_flags: Vec<u32>,
    /// IUSE tokens, verbatim.
    pub iuse: Vec<String>,
    /// The five `*DEPEND` strings in [`DependKind::ALL`] order, each optional.
    ///
    /// [`DependKind::ALL`]: crate::record::DependKind::ALL
    pub depends: [Option<String>; 5],
    /// KEYWORDS tokens.
    pub keywords: Vec<String>,
    /// LICENSE string.
    pub license: String,
    /// PROPERTIES string.
    pub properties: String,
    /// RESTRICT string.
    pub restrict: String,
    /// Token index of the repository, if recorded.
    pub repository: Option<u32>,
    /// DEFINED_PHASES tokens.
    pub defined_phases: Vec<String>,
    /// BUILD_TIME.
    pub build_time: Option<u64>,
    /// BUILD_ID.
    pub build_id: Option<u64>,
    /// The per-package counter value.
    pub counter: u64,
    /// CHOST.
    pub chost: String,
    /// Provided sonames as `(bucket token, soname token)` index pairs.
    pub provides: Vec<(u32, u32)>,
    /// Required sonames as `(bucket token, soname token)` index pairs.
    pub requires: Vec<(u32, u32)>,
    /// CONTENTS entries.
    pub contents: Vec<WireEntry>,
    /// The saved build-environment reference, if recorded.
    pub environment: Option<WireEnv>,
}

/// A CONTENTS entry on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireEntry {
    /// The installed path.
    pub path: String,
    /// The entry kind.
    pub kind: WireEntryKind,
}

/// The on-disk CONTENTS entry kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum WireEntryKind {
    /// A regular file.
    Obj {
        /// md5 digest.
        md5: String,
        /// mtime.
        mtime: i64,
    },
    /// A symlink.
    Sym {
        /// Link target.
        target: String,
        /// mtime.
        mtime: i64,
    },
    /// A directory.
    Dir,
}

/// The saved build-environment reference on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireEnv {
    /// BLAKE3 digest of the blob.
    pub digest: String,
    /// The compressed environment bytes.
    pub blob: Vec<u8>,
}
