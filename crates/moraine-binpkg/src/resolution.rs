//! Binary packages as solver candidates and their compatibility verdicts.
//!
//! A binary package competes with ebuild versions for the same atom. Whether a
//! binary is offered at all follows a usepkg-style [`UsepkgPolicy`], and whether
//! an offered binary is accepted follows the compatibility checks in
//! [`check_compatibility`]: recorded USE against the configuration's selected
//! USE, CHOST against the target, and soname REQUIRES against the PROVIDES of
//! the resolved set or installed store. A binary failing any check is rejected
//! and the ebuild candidate is used instead.

use std::collections::BTreeSet;

use moraine_version::Version;

use crate::metadata::MetadataMap;

/// The usepkg-style selection mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsepkgMode {
    /// Binaries are never offered; only source candidates exist.
    Disabled,
    /// Binaries are offered alongside source candidates.
    Eligible,
    /// Binaries are preferred over source when a compatible one exists.
    Preferred,
    /// Binaries are required; no source candidate is substituted.
    Required,
}

/// The usepkg policy: a mode plus per-atom include and exclude sets.
///
/// The include and exclude sets hold `category/package` keys. An excluded atom
/// never receives a binary candidate. An included atom is treated as
/// [`UsepkgMode::Preferred`] even when the global mode is only
/// [`UsepkgMode::Eligible`].
#[derive(Debug, Clone)]
pub struct UsepkgPolicy {
    /// The global selection mode.
    pub mode: UsepkgMode,
    /// `category/package` atoms that opt into binaries beyond the global mode.
    pub include: BTreeSet<String>,
    /// `category/package` atoms that never receive a binary candidate.
    pub exclude: BTreeSet<String>,
}

impl Default for UsepkgPolicy {
    fn default() -> Self {
        Self {
            mode: UsepkgMode::Disabled,
            include: BTreeSet::new(),
            exclude: BTreeSet::new(),
        }
    }
}

/// The effective decision for one `category/package`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eligibility {
    /// No binary candidate is offered.
    None,
    /// A binary candidate is offered alongside source.
    Eligible,
    /// A binary candidate is preferred over source.
    Preferred,
    /// A binary candidate is required; no source substitution is allowed.
    Required,
}

impl UsepkgPolicy {
    /// The effective eligibility for `cp` under this policy.
    pub fn eligibility(&self, cp: &str) -> Eligibility {
        if self.exclude.contains(cp) {
            return Eligibility::None;
        }
        match self.mode {
            UsepkgMode::Disabled => {
                if self.include.contains(cp) {
                    Eligibility::Preferred
                } else {
                    Eligibility::None
                }
            }
            UsepkgMode::Eligible => {
                if self.include.contains(cp) {
                    Eligibility::Preferred
                } else {
                    Eligibility::Eligible
                }
            }
            UsepkgMode::Preferred => Eligibility::Preferred,
            UsepkgMode::Required => Eligibility::Required,
        }
    }
}

/// A binary candidate offered for an atom, carrying its recorded dependencies.
///
/// The dependencies are the strings recorded in the binary's embedded metadata,
/// not dependencies re-evaluated from the ebuild, so the resolver reasons about
/// exactly what the binary was built against.
#[derive(Debug, Clone)]
pub struct BinaryCandidate {
    /// The `category/package`.
    pub cp: String,
    /// The package version.
    pub version: Version,
    /// The binary's embedded metadata, including the recorded dependency keys.
    pub metadata: MetadataMap,
}

impl BinaryCandidate {
    /// The recorded value of a dependency key, if present.
    pub fn recorded_dep(&self, key: &str) -> Option<String> {
        self.metadata.get_str(key)
    }

    /// The binary's recorded IUSE flags, with any `+`/`-` default prefix
    /// stripped. Empty when the binary recorded no IUSE.
    pub fn recorded_iuse(&self) -> Vec<String> {
        self.metadata.iuse_flags()
    }
}

/// The target configuration a binary is checked against.
#[derive(Debug, Clone)]
pub struct TargetConfig {
    /// The target CHOST triple.
    pub chost: String,
    /// The USE flags the configuration would enable for the package.
    pub selected_use: BTreeSet<String>,
    /// USE flags forced on regardless of the recorded set.
    pub forced_use: BTreeSet<String>,
    /// USE flags masked off regardless of the recorded set.
    pub masked_use: BTreeSet<String>,
    /// The sonames provided by the resolved set or the installed store, as
    /// `(bucket, soname)` pairs. Used to satisfy a binary's soname REQUIRES.
    pub available_sonames: BTreeSet<(String, String)>,
}

/// Why a binary candidate was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The recorded USE did not match the configuration's selected USE.
    UseMismatch {
        /// Flags enabled in the binary but not wanted.
        extra: Vec<String>,
        /// Flags wanted but not enabled in the binary.
        missing: Vec<String>,
    },
    /// The recorded CHOST did not match the target CHOST.
    ChostMismatch {
        /// The binary's recorded CHOST.
        recorded: String,
        /// The target CHOST.
        target: String,
    },
    /// One or more soname REQUIRES could not be satisfied.
    UnsatisfiedSonames(Vec<String>),
}

/// The verdict of checking a binary candidate against a target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The binary is compatible and may be offered.
    Accept,
    /// The binary is incompatible; the ebuild candidate should be used.
    Reject(Rejection),
}

/// Check a binary candidate's compatibility with the target configuration.
///
/// Verifies recorded USE (accounting for forced and masked flags), CHOST, and
/// soname REQUIRES. When the binary carries no soname REQUIRES, the soname check
/// is skipped and the verdict rests on USE and CHOST. The first failing check
/// determines the rejection.
pub fn check_compatibility(candidate: &BinaryCandidate, target: &TargetConfig) -> Verdict {
    let span = tracing::debug_span!(
        "binpkg.compat.check",
        cp = candidate.cp,
        version = candidate.version.as_str()
    );
    let _enter = span.enter();

    if let Some(rejection) = check_chost(candidate, target) {
        return Verdict::Reject(rejection);
    }
    if let Some(rejection) = check_use(candidate, target) {
        return Verdict::Reject(rejection);
    }
    if let Some(rejection) = check_sonames(candidate, target) {
        return Verdict::Reject(rejection);
    }
    Verdict::Accept
}

fn check_chost(candidate: &BinaryCandidate, target: &TargetConfig) -> Option<Rejection> {
    let recorded = candidate.metadata.chost()?;
    if recorded != target.chost {
        return Some(Rejection::ChostMismatch {
            recorded,
            target: target.chost.clone(),
        });
    }
    None
}

fn check_use(candidate: &BinaryCandidate, target: &TargetConfig) -> Option<Rejection> {
    let recorded: BTreeSet<String> = candidate.metadata.use_flags().into_iter().collect();

    // The effective wanted set: selected plus forced, minus masked.
    let mut wanted = target.selected_use.clone();
    for f in &target.forced_use {
        wanted.insert(f.clone());
    }
    for f in &target.masked_use {
        wanted.remove(f);
    }

    // The effective recorded set, with masked flags ignored on both sides since
    // a masked flag cannot legitimately differ.
    let mut recorded_effective: BTreeSet<String> = recorded
        .iter()
        .filter(|f| !target.masked_use.contains(*f))
        .cloned()
        .collect();

    // Scope both sides to the binary's recorded IUSE before diffing, mirroring
    // Portage's `orig_iuse.intersection(orig_use) ^ cur_iuse.intersection(cur_use)`
    // (`lib/_emerge/depgraph.py:2978`), so implicit or expand flags outside IUSE
    // cannot spuriously reject a compatible binary. When the binary recorded no
    // IUSE, the comparison stays unscoped so the full sets are diffed.
    let iuse: BTreeSet<String> = candidate.recorded_iuse().into_iter().collect();
    if !iuse.is_empty() {
        wanted.retain(|f| iuse.contains(f));
        recorded_effective.retain(|f| iuse.contains(f));
    }

    let extra: Vec<String> = recorded_effective.difference(&wanted).cloned().collect();
    let missing: Vec<String> = wanted.difference(&recorded_effective).cloned().collect();

    if extra.is_empty() && missing.is_empty() {
        None
    } else {
        Some(Rejection::UseMismatch { extra, missing })
    }
}

fn check_sonames(candidate: &BinaryCandidate, target: &TargetConfig) -> Option<Rejection> {
    let requires = candidate.metadata.get_str(crate::metadata::KEY_REQUIRES)?;
    let needed = parse_sonames(&requires);
    if needed.is_empty() {
        return None;
    }
    let unmet: Vec<String> = needed
        .into_iter()
        .filter(|(bucket, soname)| {
            !target
                .available_sonames
                .contains(&(bucket.clone(), soname.clone()))
        })
        .map(|(_, soname)| soname)
        .collect();
    if unmet.is_empty() {
        None
    } else {
        Some(Rejection::UnsatisfiedSonames(unmet))
    }
}

/// Parse a stock `PROVIDES`/`REQUIRES` soname string into `(bucket, soname)`
/// pairs.
///
/// The format is bucket tokens (for example `x86_64:`) followed by the sonames
/// in that bucket. A soname with no preceding bucket is assigned an empty
/// bucket.
pub fn parse_sonames(raw: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut bucket = String::new();
    for token in raw.split_whitespace() {
        if let Some(name) = token.strip_suffix(':') {
            bucket = name.to_string();
        } else {
            out.push((bucket.clone(), token.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{KEY_CHOST, KEY_PROVIDES, KEY_REQUIRES, KEY_USE};

    fn candidate(use_str: &str, chost: &str) -> BinaryCandidate {
        let mut m = MetadataMap::new();
        m.set_str(KEY_USE, use_str);
        m.set_str(KEY_CHOST, chost);
        BinaryCandidate {
            cp: "dev-libs/foo".into(),
            version: Version::parse("1.2.3").unwrap(),
            metadata: m,
        }
    }

    fn target(use_flags: &[&str], chost: &str) -> TargetConfig {
        TargetConfig {
            chost: chost.into(),
            selected_use: use_flags.iter().map(|s| s.to_string()).collect(),
            forced_use: BTreeSet::new(),
            masked_use: BTreeSet::new(),
            available_sonames: BTreeSet::new(),
        }
    }

    #[test]
    fn matching_use_and_chost_accepted() {
        let c = candidate("ssl zlib", "x86_64-pc-linux-gnu");
        let t = target(&["ssl", "zlib"], "x86_64-pc-linux-gnu");
        assert_eq!(check_compatibility(&c, &t), Verdict::Accept);
    }

    #[test]
    fn mismatched_use_rejected() {
        let c = candidate("ssl", "x86_64-pc-linux-gnu");
        let t = target(&["ssl", "zlib"], "x86_64-pc-linux-gnu");
        match check_compatibility(&c, &t) {
            Verdict::Reject(Rejection::UseMismatch { missing, .. }) => {
                assert_eq!(missing, vec!["zlib".to_string()]);
            }
            other => panic!("expected use mismatch, got {other:?}"),
        }
    }

    #[test]
    fn chost_mismatch_rejected() {
        let c = candidate("ssl", "i686-pc-linux-gnu");
        let t = target(&["ssl"], "x86_64-pc-linux-gnu");
        assert!(matches!(
            check_compatibility(&c, &t),
            Verdict::Reject(Rejection::ChostMismatch { .. })
        ));
    }

    #[test]
    fn out_of_iuse_flag_does_not_reject() {
        // The recorded and wanted sets differ only on `foo`, which is not in the
        // binary's recorded IUSE, so the candidate must not be rejected.
        let mut c = candidate("ssl", "x86_64-pc-linux-gnu");
        c.metadata.set_str(crate::metadata::KEY_IUSE, "ssl zlib");
        let t = target(&["ssl", "foo"], "x86_64-pc-linux-gnu");
        assert_eq!(check_compatibility(&c, &t), Verdict::Accept);
    }

    #[test]
    fn in_iuse_mismatch_still_rejects() {
        // `zlib` is in IUSE and wanted but not recorded: a real mismatch.
        let mut c = candidate("ssl", "x86_64-pc-linux-gnu");
        c.metadata.set_str(crate::metadata::KEY_IUSE, "ssl zlib");
        let t = target(&["ssl", "zlib"], "x86_64-pc-linux-gnu");
        match check_compatibility(&c, &t) {
            Verdict::Reject(Rejection::UseMismatch { missing, .. }) => {
                assert_eq!(missing, vec!["zlib".to_string()]);
            }
            other => panic!("expected use mismatch, got {other:?}"),
        }
    }

    #[test]
    fn forced_and_masked_flags_accounted() {
        // Binary built with `ssl`; config masks `ssl` and forces `zlib`.
        let c = candidate("ssl zlib", "x86_64-pc-linux-gnu");
        let mut t = target(&["zlib"], "x86_64-pc-linux-gnu");
        t.masked_use.insert("ssl".into());
        t.forced_use.insert("zlib".into());
        assert_eq!(check_compatibility(&c, &t), Verdict::Accept);
    }

    #[test]
    fn unsatisfiable_sonames_rejected() {
        let mut c = candidate("", "x86_64-pc-linux-gnu");
        c.metadata
            .set_str(KEY_REQUIRES, "x86_64: libc.so.6 libssl.so.3");
        let mut t = target(&[], "x86_64-pc-linux-gnu");
        t.available_sonames
            .insert(("x86_64".into(), "libc.so.6".into()));
        match check_compatibility(&c, &t) {
            Verdict::Reject(Rejection::UnsatisfiedSonames(missing)) => {
                assert_eq!(missing, vec!["libssl.so.3".to_string()]);
            }
            other => panic!("expected soname rejection, got {other:?}"),
        }
    }

    #[test]
    fn imported_provider_satisfies_bucketed_requires() {
        // A binary requires `libc.so.6` in the `x86_64` bucket; an installed
        // provider imported from a Portage VDB carries the same bucket label.
        let mut c = candidate("", "x86_64-pc-linux-gnu");
        c.metadata.set_str(KEY_REQUIRES, "x86_64: libc.so.6");
        let mut t = target(&[], "x86_64-pc-linux-gnu");
        t.available_sonames
            .insert(("x86_64".into(), "libc.so.6".into()));
        assert_eq!(check_compatibility(&c, &t), Verdict::Accept);
    }

    #[test]
    fn absent_soname_data_falls_back() {
        // No REQUIRES recorded: soname check is skipped, USE/CHOST decide.
        let c = candidate("ssl", "x86_64-pc-linux-gnu");
        let _ = c.metadata.get_str(KEY_PROVIDES); // none present
        let t = target(&["ssl"], "x86_64-pc-linux-gnu");
        assert_eq!(check_compatibility(&c, &t), Verdict::Accept);
    }

    #[test]
    fn policy_exclude_disables_binary() {
        let mut policy = UsepkgPolicy {
            mode: UsepkgMode::Eligible,
            ..Default::default()
        };
        policy.exclude.insert("dev-libs/foo".into());
        assert_eq!(policy.eligibility("dev-libs/foo"), Eligibility::None);
        assert_eq!(policy.eligibility("dev-libs/bar"), Eligibility::Eligible);
    }

    #[test]
    fn policy_disabled_offers_nothing() {
        let policy = UsepkgPolicy::default();
        assert_eq!(policy.eligibility("dev-libs/foo"), Eligibility::None);
    }

    #[test]
    fn policy_include_promotes_under_disabled() {
        let mut policy = UsepkgPolicy::default();
        policy.include.insert("dev-libs/foo".into());
        assert_eq!(policy.eligibility("dev-libs/foo"), Eligibility::Preferred);
    }

    #[test]
    fn policy_required_offers_required() {
        let policy = UsepkgPolicy {
            mode: UsepkgMode::Required,
            ..Default::default()
        };
        assert_eq!(policy.eligibility("dev-libs/foo"), Eligibility::Required);
    }
}
