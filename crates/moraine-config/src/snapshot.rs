//! The immutable resolved configuration snapshot and its cache.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use moraine_atom::PackageRef;
use moraine_common::Interner;

use crate::keywords::KeywordsManager;
use crate::license::{LicenseManager, LicenseReq};
use crate::profile::ProfileStack;
use crate::use_resolution::{EffectiveUse, UseManager};
use crate::visibility::{
    KeywordResult, MaskManager, MaskPattern, MaskReason, ProvidedManager, accept_keywords,
};

/// A `package.env` overlay entry: a cp pattern and the ordered `(key, value)`
/// assignments from every referenced `/etc/portage/env/` file, applied when the
/// pattern matches a package.
#[derive(Debug, Clone)]
pub struct PkgEnvEntry {
    /// The cp pattern (concrete atom or extended wildcard) the entry applies to.
    pub pattern: MaskPattern,
    /// The ordered variable assignments contributed by the matched env files.
    pub vars: Vec<(String, String)>,
}

/// A `package.bashrc` overlay entry: a cp pattern and the bashrc files selected
/// for matching packages.
#[derive(Debug, Clone)]
pub struct PkgBashrcEntry {
    /// The cp pattern the entry applies to.
    pub pattern: MaskPattern,
    /// The bashrc files to source for matching packages.
    pub files: Vec<PathBuf>,
}

/// The combined visibility verdict for a package: masking, then keywords.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    /// The package is installable.
    Visible,
    /// The package is hard-masked; the string is the responsible mask token.
    HardMasked(String),
    /// The package needs a testing (`~arch`) keyword to be accepted.
    NeedsKeyword,
    /// The package has broken or empty keywords and needs `**`.
    NeedsDoubleStar,
}

/// An immutable, queryable snapshot of resolved configuration. Query methods
/// take `&self` and never mutate the snapshot.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// The resolved profile stack.
    pub profile: ProfileStack,
    /// The stable architecture keyword (for example `amd64`).
    pub arch: String,
    accepted_keywords: BTreeSet<String>,
    use_manager: UseManager,
    mask_manager: MaskManager,
    license_manager: LicenseManager,
    keywords_manager: KeywordsManager,
    provided: ProvidedManager,
    system: Vec<String>,
    world: Vec<String>,
    /// The shared interner, used to resolve candidate symbols for extended-cp
    /// (partial glob) matching at query time.
    interner: Arc<Interner>,
    pkg_env: Vec<PkgEnvEntry>,
    profile_bashrcs: Vec<PathBuf>,
    user_bashrc: Option<PathBuf>,
    package_bashrc: Vec<PkgBashrcEntry>,
}

impl ResolvedConfig {
    /// Assemble a snapshot from the loaded managers.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        profile: ProfileStack,
        arch: String,
        accepted_keywords: BTreeSet<String>,
        use_manager: UseManager,
        mask_manager: MaskManager,
        license_manager: LicenseManager,
        keywords_manager: KeywordsManager,
        provided: ProvidedManager,
        system: Vec<String>,
        world: Vec<String>,
        interner: Arc<Interner>,
    ) -> Self {
        ResolvedConfig {
            profile,
            arch,
            accepted_keywords,
            use_manager,
            mask_manager,
            license_manager,
            keywords_manager,
            provided,
            system,
            world,
            interner,
            pkg_env: Vec::new(),
            profile_bashrcs: Vec::new(),
            user_bashrc: None,
            package_bashrc: Vec::new(),
        }
    }

    /// Attach the per-package `package.env` overlay entries.
    pub fn with_pkg_env(mut self, pkg_env: Vec<PkgEnvEntry>) -> Self {
        self.pkg_env = pkg_env;
        self
    }

    /// Attach the bashrc sourcing inputs: the profile-node `profile.bashrc`
    /// files (stack order), the user `PORTAGE_BASHRC` (`/etc/portage/bashrc`),
    /// and the `package.bashrc` per-package selections.
    pub fn with_bashrcs(
        mut self,
        profile_bashrcs: Vec<PathBuf>,
        user_bashrc: Option<PathBuf>,
        package_bashrc: Vec<PkgBashrcEntry>,
    ) -> Self {
        self.profile_bashrcs = profile_bashrcs;
        self.user_bashrc = user_bashrc;
        self.package_bashrc = package_bashrc;
        self
    }

    /// The ordered `(key, value)` `package.env` overlay for a package, the
    /// concatenation of every matching entry's variables (profile/user order,
    /// least specific first).
    pub fn package_env_overlay(&self, pkg: &PackageRef<'_>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for entry in &self.pkg_env {
            if entry.pattern.matches(pkg, &self.interner) {
                out.extend(entry.vars.iter().cloned());
            }
        }
        out
    }

    /// The bashrc files to source for a package, in Portage order: each profile
    /// node's `profile.bashrc`, then the user `PORTAGE_BASHRC`, then the matching
    /// `package.bashrc`-selected files.
    pub fn bashrc_files(&self, pkg: &PackageRef<'_>) -> Vec<PathBuf> {
        let mut out = self.profile_bashrcs.clone();
        if let Some(user) = &self.user_bashrc {
            out.push(user.clone());
        }
        for entry in &self.package_bashrc {
            if entry.pattern.matches(pkg, &self.interner) {
                out.extend(entry.files.iter().cloned());
            }
        }
        out
    }

    /// The package's effective `KEYWORDS` after applying profile
    /// `package.keywords`, given the ebuild's raw `KEYWORDS`.
    pub fn stacked_keywords(
        &self,
        pkg: &PackageRef<'_>,
        ebuild_keywords: &[String],
    ) -> Vec<String> {
        self.keywords_manager
            .stacked_keywords(pkg, ebuild_keywords, &self.interner)
    }

    /// The per-package accepted keywords matching `pkg` from
    /// `package.accept_keywords` (and the deprecated `package.keywords`).
    pub fn package_keywords(&self, pkg: &PackageRef<'_>) -> Vec<String> {
        self.keywords_manager.pkeywords(pkg, &self.interner)
    }

    /// The licenses of `reduced` (a USE-reduced `LICENSE`) that are not accepted
    /// for `pkg`. An empty result means the license is acceptable.
    pub fn missing_licenses(&self, reduced: &LicenseReq, pkg: &PackageRef<'_>) -> BTreeSet<String> {
        self.license_manager
            .missing_licenses(reduced, pkg, &self.interner)
    }

    /// The effective USE for a package, given its raw `IUSE` tokens (with `+`/`-`
    /// default prefixes) so defaults are applied. `restrict_test` is whether the
    /// package's `RESTRICT` contains `test`, suppressing the `FEATURES=test`
    /// injection.
    pub fn effective_use(
        &self,
        pkg: &PackageRef<'_>,
        iuse: &[String],
        stable: bool,
        restrict_test: bool,
    ) -> EffectiveUse {
        self.use_manager
            .effective_use(pkg, iuse, stable, restrict_test)
    }

    /// Whether a package is masked.
    pub fn is_masked(&self, pkg: &PackageRef<'_>) -> bool {
        self.mask_manager.is_masked(pkg, &self.interner)
    }

    /// The structured masking reason for a package, naming the responsible mask
    /// token when hard-masked.
    pub fn mask_reason(&self, pkg: &PackageRef<'_>) -> MaskReason {
        self.mask_manager.reason(pkg, &self.interner)
    }

    /// The combined visibility verdict, keeping hard-mask and keyword reasons
    /// distinct: a package is hard-masked first, otherwise judged by keywords.
    pub fn visibility(
        &self,
        pkg: &PackageRef<'_>,
        keywords: &[String],
        extra: &[String],
    ) -> Visibility {
        if let MaskReason::HardMasked(atom) = self.mask_manager.reason(pkg, &self.interner) {
            return Visibility::HardMasked(atom);
        }
        match self.keyword_result(keywords, extra) {
            KeywordResult::Accepted => Visibility::Visible,
            KeywordResult::NeedsKeyword => Visibility::NeedsKeyword,
            KeywordResult::NeedsDoubleStar => Visibility::NeedsDoubleStar,
        }
    }

    /// Whether a package is provided externally.
    pub fn is_provided(&self, pkg: &PackageRef<'_>) -> bool {
        self.provided.is_provided(pkg)
    }

    /// Decide keyword acceptance for a package, given its `KEYWORDS` and any
    /// per-package extra accepted keywords.
    pub fn keyword_result(&self, keywords: &[String], extra: &[String]) -> KeywordResult {
        if extra.is_empty() {
            return accept_keywords(keywords, &self.accepted_keywords, &self.arch);
        }
        // Fold the per-package tokens onto the cloned accepted set with
        // incremental semantics, mirroring `KeywordsManager._getEgroups`: `-*`
        // clears the set, a `-keyword` token removes that keyword, and a plain
        // keyword is added.
        let mut accepted = self.accepted_keywords.clone();
        for kw in extra {
            if kw.is_empty() {
                // A bare per-package atom accepts the testing keyword for every
                // stable arch already in the effective ACCEPT_KEYWORDS, not only
                // the profile arch.
                let testing: Vec<String> = self
                    .accepted_keywords
                    .iter()
                    .filter(|k| k.chars().next().is_some_and(|c| c.is_ascii_alphanumeric()))
                    .map(|k| format!("~{k}"))
                    .collect();
                accepted.extend(testing);
            } else if kw == "-*" {
                accepted.clear();
            } else if let Some(rest) = kw.strip_prefix('-') {
                accepted.remove(rest);
            } else {
                accepted.insert(kw.clone());
            }
        }
        accept_keywords(keywords, &accepted, &self.arch)
    }

    /// The `@system` set members.
    pub fn system(&self) -> &[String] {
        &self.system
    }

    /// The `@world` set members.
    pub fn world(&self) -> &[String] {
        &self.world
    }
}

/// A fingerprint-keyed cache for a resolved configuration snapshot.
///
/// The fingerprint is the set of input paths and their modification times, so a
/// cache hit avoids re-parsing and any edit to an input invalidates it.
#[derive(Debug, Default)]
pub struct ConfigCache {
    fingerprint: Option<Vec<(PathBuf, Option<SystemTime>)>>,
    snapshot: Option<Arc<ResolvedConfig>>,
}

impl ConfigCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached snapshot if the inputs are unchanged, otherwise call
    /// `build`, cache its result, and return it.
    pub fn load<F>(&mut self, inputs: &[PathBuf], build: F) -> Arc<ResolvedConfig>
    where
        F: FnOnce() -> ResolvedConfig,
    {
        let fp = fingerprint(inputs);
        if self.fingerprint.as_ref() == Some(&fp)
            && let Some(snapshot) = &self.snapshot
        {
            return Arc::clone(snapshot);
        }
        let snapshot = Arc::new(build());
        self.fingerprint = Some(fp);
        self.snapshot = Some(Arc::clone(&snapshot));
        snapshot
    }
}

fn fingerprint(inputs: &[PathBuf]) -> Vec<(PathBuf, Option<SystemTime>)> {
    inputs
        .iter()
        .map(|p| {
            let mtime = std::fs::metadata(p).and_then(|m| m.modified()).ok();
            (p.clone(), mtime)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::use_resolution::UseManager;
    use crate::visibility::{MaskManager, ProvidedManager};

    fn empty_config() -> ResolvedConfig {
        ResolvedConfig::new(
            ProfileStack::default(),
            "amd64".to_owned(),
            BTreeSet::new(),
            UseManager::default(),
            MaskManager::new(),
            LicenseManager::default(),
            KeywordsManager::new(),
            ProvidedManager::new(),
            Vec::new(),
            Vec::new(),
            Arc::new(Interner::new()),
        )
    }

    #[test]
    fn bare_atom_extra_accepts_testing_for_accepted_arches() {
        let cfg = ResolvedConfig::new(
            ProfileStack::default(),
            "amd64".to_owned(),
            ["amd64".to_owned()].into_iter().collect(),
            UseManager::default(),
            MaskManager::new(),
            LicenseManager::default(),
            KeywordsManager::new(),
            ProvidedManager::new(),
            Vec::new(),
            Vec::new(),
            Arc::new(Interner::new()),
        );
        // A bare per-package atom (empty extra) accepts ~amd64.
        assert_eq!(
            cfg.keyword_result(&["~amd64".to_owned()], &["".to_owned()]),
            KeywordResult::Accepted
        );
        // Without the bare-atom entry, ~amd64 is not accepted.
        assert_eq!(
            cfg.keyword_result(&["~amd64".to_owned()], &[]),
            KeywordResult::NeedsKeyword
        );
    }

    #[test]
    fn negative_pkeyword_tokens_fold_incrementally() {
        let cfg = ResolvedConfig::new(
            ProfileStack::default(),
            "amd64".to_owned(),
            ["amd64".to_owned()].into_iter().collect(),
            UseManager::default(),
            MaskManager::new(),
            LicenseManager::default(),
            KeywordsManager::new(),
            ProvidedManager::new(),
            Vec::new(),
            Vec::new(),
            Arc::new(Interner::new()),
        );
        // `-* ~x86` clears the inherited amd64 acceptance, so a stable amd64
        // build is rejected on keyword grounds.
        assert_ne!(
            cfg.keyword_result(&["amd64".to_owned()], &["-*".to_owned(), "~x86".to_owned()]),
            KeywordResult::Accepted
        );
        // `-amd64` discards amd64, so a stable amd64 build is not accepted.
        assert_ne!(
            cfg.keyword_result(&["amd64".to_owned()], &["-amd64".to_owned()]),
            KeywordResult::Accepted
        );
    }

    #[test]
    fn cache_hit_avoids_rebuild_and_edit_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("make.conf");
        std::fs::write(&file, "USE=\"a\"\n").unwrap();
        let inputs = vec![file.clone()];

        let mut cache = ConfigCache::new();
        let mut builds = 0;
        let _ = cache.load(&inputs, || {
            builds += 1;
            empty_config()
        });
        let _ = cache.load(&inputs, || {
            builds += 1;
            empty_config()
        });
        assert_eq!(builds, 1, "second load should hit the cache");

        // Editing the input changes its mtime and invalidates the cache.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&file, "USE=\"a b\"\n").unwrap();
        let _ = cache.load(&inputs, || {
            builds += 1;
            empty_config()
        });
        assert_eq!(builds, 2, "edited input should invalidate the cache");
    }

    #[test]
    fn snapshot_is_queryable() {
        let config = empty_config();
        assert_eq!(config.arch, "amd64");
        assert!(config.system().is_empty());
    }
}
