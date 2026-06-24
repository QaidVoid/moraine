//! The Moraine installed store.
//!
//! This crate owns the greenfield installed database: the on-disk store format,
//! the per-package record schema, the CONTENTS file-manifest model, an importer
//! from a stock `/var/db/pkg` tree, and the in-memory query API the resolver
//! consumes.
//!
//! The runtime read path loads the complete installed set in one bulk
//! mmap-backed pass over a single primary file, with parallel record decode, and
//! never walks a per-package directory tree. Single-package changes append to a
//! delta journal; compaction folds the journal back into the primary file.
//!
//! Symbols are per-[`Interner`] and not stable across runs, so the store
//! serializes strings and a string token table, never [`Symbol`]s or parsed
//! ASTs. A fresh interner is built at load and the recorded `*DEPEND` strings are
//! parsed into ASTs in memory, with the original string retained for round-trip
//! fidelity including any `:=` slot or sub-slot binding.
//!
//! [`Interner`]: moraine_common::Interner
//! [`Symbol`]: moraine_common::Symbol

mod codec;
mod journal;
mod wire;

pub mod contents;
pub mod error;
pub mod import;
pub mod query;
pub mod record;
pub mod soname;
pub mod store;

pub use contents::{Contents, Entry, EntryKind};
pub use error::VdbError;
pub use import::import_vdb;
pub use query::{Installed, SlotBinding};
pub use record::{Depend, DependKind, DependSet, EnvironmentRef, PackageRecord, Slot};
pub use soname::{Provides, Requires, SonameEntry};
pub use store::{Store, StorePaths};
