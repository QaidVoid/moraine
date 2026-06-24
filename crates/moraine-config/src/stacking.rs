//! Incremental variable stacking, mirroring stock `stack_lists`.
//!
//! Incremental variables (USE, ACCEPT_KEYWORDS, and the like) accumulate across
//! the profile chain and `make.conf`: a bare token adds, a `-token` removes all
//! earlier equal tokens, and `-*` clears the accumulator. Non-incremental
//! variables are replaced rather than stacked.

/// Apply incremental stacking to `tokens` in order, returning the accumulated
/// list with order preserved and duplicates collapsed.
pub fn stack_incremental<'a, I>(tokens: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut acc: Vec<String> = Vec::new();
    for token in tokens {
        if token.is_empty() {
            continue;
        }
        if token == "-*" {
            acc.clear();
        } else if let Some(rest) = token.strip_prefix('-') {
            acc.retain(|existing| existing != rest);
        } else if !acc.iter().any(|existing| existing == token) {
            acc.push(token.to_owned());
        }
    }
    acc
}

/// Apply incremental stacking to whitespace-separated layers in order. Each
/// layer is the value of the variable at one profile node or `make.conf`.
pub fn stack_layers<'a, I>(layers: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut acc: Vec<String> = Vec::new();
    for layer in layers {
        for token in layer.split_whitespace() {
            if token == "-*" {
                acc.clear();
            } else if let Some(rest) = token.strip_prefix('-') {
                acc.retain(|existing| existing != rest);
            } else if !acc.iter().any(|existing| existing == token) {
                acc.push(token.to_owned());
            }
        }
    }
    acc
}

/// The set of variables treated as incremental, mirroring stock `INCREMENTALS`.
pub const INCREMENTALS: &[&str] = &[
    "USE",
    "USE_EXPAND",
    "USE_EXPAND_HIDDEN",
    "USE_EXPAND_IMPLICIT",
    "USE_EXPAND_UNPREFIXED",
    "IUSE_IMPLICIT",
    "FEATURES",
    "ACCEPT_KEYWORDS",
    "ACCEPT_LICENSE",
    "ACCEPT_PROPERTIES",
    "ACCEPT_RESTRICT",
    "CONFIG_PROTECT",
    "CONFIG_PROTECT_MASK",
    "PROFILE_ONLY_VARIABLES",
    "ENV_UNSET",
];

/// Whether `name` is an incremental variable.
pub fn is_incremental(name: &str) -> bool {
    INCREMENTALS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_token_removes_earlier() {
        assert_eq!(stack_incremental(["a", "b", "-a"]), vec!["b"]);
    }

    #[test]
    fn wildcard_resets() {
        assert_eq!(stack_incremental(["a", "b", "-*", "c"]), vec!["c"]);
    }

    #[test]
    fn duplicates_collapse() {
        assert_eq!(stack_incremental(["a", "b", "a"]), vec!["a", "b"]);
    }

    #[test]
    fn layers_stack_in_order() {
        assert_eq!(stack_layers(["a b", "-a c", "d"]), vec!["b", "c", "d"]);
    }

    #[test]
    fn incremental_membership() {
        assert!(is_incremental("USE"));
        assert!(is_incremental("ACCEPT_KEYWORDS"));
        assert!(!is_incremental("CHOST"));
    }
}
