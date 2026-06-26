//! The available-packages metadata database for the Moraine package manager.
//!
//! Non-goal: old-style `profiles/virtuals` is not supported. Modern Gentoo
//! expresses virtuals as `virtual/*` packages, so the legacy text-file virtuals
//! mechanism is intentionally omitted rather than implemented.
//!
//! This crate is the `portdbapi` equivalent of the rewrite. It answers the
//! question the solver asks millions of times: given an atom, which package
//! versions exist, and what are their dependencies, slots, keywords, and USE
//! flags? It does so in layers:
//!
//! - [`discovery`]: parse `repos.conf`, `layout.conf`, and `profiles/repo_name`,
//!   resolve masters inheritance and `priority` into one deterministic
//!   repository order, and build per-repository eclass search paths.
//! - [`store`]: a greenfield, mmap-backed on-disk format holding the resolution
//!   subset of metadata per ebuild version, with dependency variables stored as
//!   raw text and parsed into [`moraine_atom`] ASTs once at load time so the
//!   resolver never re-parses.
//! - [`import`]: a parallel, eclass-validated, incremental importer from stock
//!   md5-cache into the store.
//! - [`query`]: the atom-to-candidate-version query API the solver consumes.
//!
//! # Interning and on-disk stability
//!
//! [`moraine_common::Symbol`] values are per-interner and not stable across
//! runs, so the store never serializes symbols or parsed ASTs. It writes raw
//! dependency strings and structured string fields, then on load builds its own
//! interner and parses the strings into ASTs held in memory.

pub mod discovery;
pub mod error;
pub mod flatlist;
pub mod import;
pub mod query;
pub mod store;
pub mod updates;

use std::collections::HashMap;
use std::path::Path;

use tracing::instrument;

pub use discovery::{RepoConfig, RepoSet, discover};
pub use error::{DiscoveryError, ImportError, RepoError, Result};
pub use import::{ImportIssue, ImportReport, import_repo};
pub use query::{Candidate, RepoIndex, RepoStore};
pub use store::{FORMAT_VERSION, LoadedEntry, LoadedStore, StoredEntry};
pub use updates::{
    Cp, UpdateCommand, UpdateError, UpdateFile, grab_updates, load_mtimes, parse_updates,
    store_mtimes,
};

/// Build the incremental-reimport index from on-disk store entries, which retain
/// `_mtime_` and `_md5_`. The importer reuses an entry whose `_mtime_` and
/// `_md5_` match the source cache file.
pub fn previous_index(entries: &[StoredEntry]) -> HashMap<(String, String, String), StoredEntry> {
    entries
        .iter()
        .map(|e| {
            (
                (e.category.clone(), e.package.clone(), e.version.clone()),
                e.clone(),
            )
        })
        .collect()
}

/// Discover repositories, import each into its store file, and load them into a
/// queryable [`RepoIndex`] in the discovered order.
///
/// `repos_conf` is the `repos.conf` file or directory. `store_dir` is where each
/// repository's `<name>.mrepo` store file is written. If a store file already
/// exists and is valid and current, its entries seed an incremental reimport so
/// only changed entries are re-parsed.
#[instrument(skip_all)]
pub fn build_index(repos_conf: impl AsRef<Path>, store_dir: impl AsRef<Path>) -> Result<RepoIndex> {
    build_index_with(repos_conf, store_dir, None)
}

/// Build the index parsing every repository store against a shared interner.
///
/// When `interner` is `Some`, every repository store parses its entries against
/// that one interner, so atoms parsed elsewhere against the same interner (for
/// example a `ResolvedConfig`) compare equal to the stores' symbols. With `None`
/// each store gets its own interner, matching [`build_index`].
#[instrument(skip_all)]
pub fn build_index_with(
    repos_conf: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    interner: Option<std::sync::Arc<moraine_common::Interner>>,
) -> Result<RepoIndex> {
    let repo_set = discover(repos_conf)?;
    let store_dir = store_dir.as_ref();
    std::fs::create_dir_all(store_dir).map_err(|source| {
        RepoError::Common(moraine_common::CommonError::Io {
            path: store_dir.to_path_buf(),
            source,
        })
    })?;

    let mut repos = Vec::new();
    for cfg in repo_set.ordered() {
        let store_path = store_dir.join(format!("{}.mrepo", cfg.name));

        // Seed incremental reimport from a valid existing store.
        let previous = match store::read_entries(&store_path) {
            Ok(prev) => previous_index(&prev),
            Err(_) => HashMap::new(),
        };

        let report = import_repo(&repo_set, &cfg.name, &previous)?;
        store::write_store(&store_path, report.entries)?;
        let store = match &interner {
            Some(shared) => LoadedStore::load_with(&store_path, std::sync::Arc::clone(shared))?,
            None => LoadedStore::load(&store_path)?,
        };
        repos.push(RepoStore {
            name: cfg.name.clone(),
            store,
        });
    }

    Ok(RepoIndex::new(repos))
}
