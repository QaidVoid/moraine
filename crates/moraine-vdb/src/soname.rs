//! Soname linkage: the `PROVIDES` and `REQUIRES` of an installed package.
//!
//! These mirror the stock `PROVIDES`/`REQUIRES` aux keys derived from
//! `NEEDED.ELF.2`. The resolver uses them for soname dependency satisfaction:
//! which installed packages provide a soname and which sonames a package needs.
//! Sonames are interned so lookups compare ids rather than strings.

use moraine_common::Symbol;

/// The sonames a package provides, grouped by ELF ABI bucket.
///
/// Stock Portage groups sonames by a multilib category token (for example
/// `x86_64` or `x86_32`). The bucket is preserved so a 32-bit soname does not
/// satisfy a 64-bit requirement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Provides {
    /// The provided sonames as `(bucket, soname)` interned pairs.
    pub entries: Vec<SonameEntry>,
}

/// The sonames a package requires, grouped by ELF ABI bucket.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Requires {
    /// The required sonames as `(bucket, soname)` interned pairs.
    pub entries: Vec<SonameEntry>,
}

/// A single soname tagged with its ABI bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SonameEntry {
    /// The interned ABI bucket token (for example `x86_64`).
    pub bucket: Symbol,
    /// The interned soname (for example `libc.so.6`).
    pub soname: Symbol,
}

impl Provides {
    /// Whether this package provides `soname` in any bucket.
    pub fn provides(&self, soname: Symbol) -> bool {
        self.entries.iter().any(|e| e.soname == soname)
    }

    /// Whether this package provides `soname` within `bucket`.
    pub fn provides_in(&self, bucket: Symbol, soname: Symbol) -> bool {
        self.entries
            .iter()
            .any(|e| e.bucket == bucket && e.soname == soname)
    }
}

impl Requires {
    /// The required sonames, ignoring bucket.
    pub fn sonames(&self) -> impl Iterator<Item = Symbol> + '_ {
        self.entries.iter().map(|e| e.soname)
    }
}
