//! EAPI feature-flag table.
//!
//! Mirrors stock Portage `_get_eapi_attrs` (`lib/portage/eapi.py`) as a static,
//! compile-time table from EAPI integer (0 through [`LATEST`]) to the feature
//! predicates the parsers depend on. An unrecognized or unsupported EAPI string
//! degrades to the [`PERMISSIVE`] fallback rather than panicking, matching the
//! stock behavior when `eapi` is `None`.

/// The highest EAPI version this crate knows about.
pub const LATEST: u8 = 9;

/// The per-EAPI feature predicates the parsers and resolver consult.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EapiFeatures {
    /// Slot dependencies (`:slot`) are allowed. EAPI 1+.
    pub slot_deps: bool,
    /// IUSE default-enabled (`+flag`) tokens. EAPI 1+.
    pub iuse_defaults: bool,
    /// USE dependencies (`[flag]`) are allowed. EAPI 2+.
    pub use_deps: bool,
    /// `SRC_URI` arrows (`a -> b`). EAPI 2+.
    pub src_uri_arrows: bool,
    /// Strong blockers (`!!`). EAPI 2+.
    pub strong_blocks: bool,
    /// Prefix support. EAPI 3+.
    pub prefix: bool,
    /// `REQUIRED_USE`. EAPI 4+.
    pub required_use: bool,
    /// USE-dependency defaults (`(+)`/`(-)`). EAPI 4+.
    pub use_dep_defaults: bool,
    /// Slot operators (`:=`, `:*`, `:slot=`). EAPI 5+.
    pub slot_operator: bool,
    /// `IUSE_EFFECTIVE` / implicit IUSE. EAPI 5+.
    pub iuse_effective: bool,
    /// `REQUIRED_USE` `?? ( )` at-most-one-of groups. EAPI 5+.
    pub required_use_at_most_one_of: bool,
    /// `BDEPEND` (build-host dependencies). EAPI 7+.
    pub bdepend: bool,
    /// `IDEPEND` (install-time dependencies). EAPI 8+.
    pub idepend: bool,
    /// Stable-use masking/forcing semantics. EAPI 9+.
    pub use_stable: bool,
    /// Repository dependencies (`::repo` in atoms). Never set for a real EAPI;
    /// only the permissive fallback enables it.
    pub repo_deps: bool,
}

const fn for_level(n: u8) -> EapiFeatures {
    EapiFeatures {
        slot_deps: n >= 1,
        iuse_defaults: n >= 1,
        use_deps: n >= 2,
        src_uri_arrows: n >= 2,
        strong_blocks: n >= 2,
        prefix: n >= 3,
        required_use: n >= 4,
        use_dep_defaults: n >= 4,
        slot_operator: n >= 5,
        iuse_effective: n >= 5,
        required_use_at_most_one_of: n >= 5,
        bdepend: n >= 7,
        idepend: n >= 8,
        use_stable: n >= 9,
        repo_deps: false,
    }
}

/// The permissive fallback for unknown or unsupported EAPIs. Every gated feature
/// is allowed so that parsing of corrupt or future metadata can still proceed.
pub const PERMISSIVE: EapiFeatures = EapiFeatures {
    slot_deps: true,
    iuse_defaults: true,
    use_deps: true,
    src_uri_arrows: true,
    strong_blocks: true,
    prefix: true,
    required_use: true,
    use_dep_defaults: true,
    slot_operator: true,
    iuse_effective: true,
    required_use_at_most_one_of: true,
    bdepend: true,
    idepend: true,
    use_stable: true,
    repo_deps: true,
};

static TABLE: [EapiFeatures; (LATEST + 1) as usize] = [
    for_level(0),
    for_level(1),
    for_level(2),
    for_level(3),
    for_level(4),
    for_level(5),
    for_level(6),
    for_level(7),
    for_level(8),
    for_level(9),
];

/// Parse an EAPI string to its integer level when it is a supported numeric
/// EAPI, otherwise return `None`.
pub fn level(eapi: &str) -> Option<u8> {
    match eapi.parse::<u8>() {
        Ok(n) if n <= LATEST => Some(n),
        _ => None,
    }
}

/// Return the feature struct for a numeric EAPI level. Levels above [`LATEST`]
/// return [`PERMISSIVE`].
pub fn features_for_level(n: u8) -> EapiFeatures {
    TABLE.get(n as usize).copied().unwrap_or(PERMISSIVE)
}

/// Return the feature struct for an EAPI string. An unrecognized or unsupported
/// EAPI yields [`PERMISSIVE`] rather than panicking.
pub fn features_for(eapi: &str) -> EapiFeatures {
    match level(eapi) {
        Some(n) => TABLE[n as usize],
        None => PERMISSIVE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_features_gate_at_one_and_five() {
        assert!(!features_for_level(0).slot_deps);
        assert!(features_for_level(1).slot_deps);
        assert!(!features_for_level(4).slot_operator);
        assert!(features_for_level(5).slot_operator);
    }

    #[test]
    fn use_and_blocker_features_gate_at_two() {
        assert!(!features_for_level(1).use_deps);
        assert!(features_for_level(2).use_deps);
        assert!(!features_for_level(1).strong_blocks);
        assert!(features_for_level(2).strong_blocks);
    }

    #[test]
    fn use_dep_defaults_gate_at_four() {
        assert!(!features_for_level(3).use_dep_defaults);
        assert!(features_for_level(4).use_dep_defaults);
    }

    #[test]
    fn bdepend_idepend_use_stable_gate_at_seven_eight_nine() {
        assert!(!features_for_level(6).bdepend);
        assert!(features_for_level(7).bdepend);
        assert!(!features_for_level(7).idepend);
        assert!(features_for_level(8).idepend);
        assert!(!features_for_level(8).use_stable);
        assert!(features_for_level(9).use_stable);
    }

    #[test]
    fn real_eapis_never_enable_repo_deps() {
        for n in 0..=LATEST {
            assert!(!features_for_level(n).repo_deps);
        }
    }

    #[test]
    fn unknown_eapi_is_permissive_and_does_not_panic() {
        assert_eq!(features_for("banana"), PERMISSIVE);
        assert_eq!(features_for("99"), PERMISSIVE);
        assert_eq!(features_for(""), PERMISSIVE);
        assert!(features_for("8").idepend);
    }
}
