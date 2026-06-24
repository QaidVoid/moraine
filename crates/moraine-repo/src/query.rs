//! The package query API, the `portdbapi` equivalent over loaded stores.
//!
//! A [`RepoIndex`] holds one [`crate::store::LoadedStore`] per repository in the
//! discovered search order. It enumerates `cp` and per-`cp` versions in
//! [`moraine_version`] order, matches a [`moraine_atom::Atom`] (version
//! operators, slot, slot-operator) to candidate versions, and fetches pre-parsed
//! metadata. Candidates carry their originating repository. The store is read
//! only and shared by `&self`, so queries run concurrently without locking.
//!
//! Masking, keyword acceptance, and USE-dependency filtering are deliberately
//! left to `moraine-config` and the solver. This layer applies only the
//! structural atom constraints.

use moraine_atom::{Atom, PackageRef};
use moraine_common::Symbol;
use moraine_version::Version;
use tracing::instrument;

use crate::store::{LoadedEntry, LoadedStore};

/// A single repository's loaded store paired with its name and position in the
/// discovered order.
#[derive(Debug)]
pub struct RepoStore {
    /// The repository name.
    pub name: String,
    /// The loaded, read-only store.
    pub store: LoadedStore,
}

/// An ordered set of per-repository stores forming the available-packages
/// database. Repositories appear in the discovered search order.
#[derive(Debug)]
pub struct RepoIndex {
    repos: Vec<RepoStore>,
}

/// A candidate ebuild version matched from a query, tagged with its repository.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// The repository the candidate came from.
    pub repo: &'a str,
    /// The index of the repository in the discovered order.
    pub repo_order: usize,
    /// The matched entry with pre-parsed metadata.
    pub entry: &'a LoadedEntry,
}

impl RepoIndex {
    /// Build an index from per-repository stores already in search order.
    pub fn new(repos: Vec<RepoStore>) -> Self {
        Self { repos }
    }

    /// The repositories in search order.
    pub fn repos(&self) -> &[RepoStore] {
        &self.repos
    }

    /// Enumerate every `(category, package)` present across all repositories,
    /// with the interner that resolves the symbols. Symbols originate from each
    /// store's own interner, so resolve them via that store.
    pub fn catalog(&self) -> impl Iterator<Item = (&RepoStore, Symbol, Symbol)> + '_ {
        self.repos
            .iter()
            .flat_map(|rs| rs.store.catalog().map(move |(c, p)| (rs, c, p)))
    }

    /// Match an atom to candidate versions across all repositories, honoring the
    /// discovered repository order. The atom must be parsed against each store's
    /// interner; use [`RepoIndex::match_atom_in`] when an atom is bound to a
    /// single store's interner.
    ///
    /// Returns candidates in repository order, then ascending version order.
    #[instrument(skip(self, atom_text))]
    pub fn match_atom_str(&self, atom_text: &str) -> Vec<Candidate<'_>> {
        let mut out = Vec::new();
        for (order, rs) in self.repos.iter().enumerate() {
            let features = moraine_eapi::PERMISSIVE;
            let Ok(atom) = Atom::parse(atom_text, features, rs.store.interner()) else {
                continue;
            };
            collect_matches(rs, order, &atom, &mut out);
        }
        out
    }

    /// Match an atom that was parsed against the interner of the repository at
    /// `repo_order`. The match is restricted to that repository.
    pub fn match_atom_in(&self, repo_order: usize, atom: &Atom) -> Vec<Candidate<'_>> {
        let mut out = Vec::new();
        if let Some(rs) = self.repos.get(repo_order) {
            collect_matches(rs, repo_order, atom, &mut out);
        }
        out
    }

    /// Fetch the entry for an exact `(category, package, version)` in a specific
    /// repository, returning its pre-parsed metadata.
    pub fn get(
        &self,
        repo_order: usize,
        category: Symbol,
        package: Symbol,
        version: &Version,
    ) -> Option<&LoadedEntry> {
        self.repos
            .get(repo_order)
            .and_then(|rs| rs.store.get(category, package, version))
    }
}

/// Collect candidate matches for `atom` within one repository store. The atom's
/// symbols must belong to that store's interner.
fn collect_matches<'a>(rs: &'a RepoStore, order: usize, atom: &Atom, out: &mut Vec<Candidate<'a>>) {
    let entries = rs.store.versions_of(atom.category(), atom.package());
    for entry in entries {
        if matches_entry(atom, entry) {
            out.push(Candidate {
                repo: &rs.name,
                repo_order: order,
                entry,
            });
        }
    }
}

/// Test whether an atom matches a loaded entry, applying category/package,
/// version operator, slot, and slot-operator constraints.
fn matches_entry(atom: &Atom, entry: &LoadedEntry) -> bool {
    let pkg = PackageRef {
        category: entry.category,
        package: entry.package,
        version: &entry.version,
        slot: Some(entry.slot),
        subslot: entry.subslot,
        repo: Some(entry.repository),
    };
    // `Atom::matches` checks cp, version operator, slot, sub-slot, and repo. A
    // bare `:*` or `:=` slot operator with no slot name imposes no slot
    // constraint here (the rebuild binding is the solver's concern), which
    // `Atom::matches` already honors since `slot()` is `None` in that case.
    atom.matches(&pkg)
}
