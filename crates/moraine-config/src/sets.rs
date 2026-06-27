//! Package sets: `@system`, `@selected`, `@world`, and file-backed user sets.

use std::collections::BTreeSet;
use std::path::Path;

use crate::error::ConfigError;

/// Incrementally stack the `packages` token lists across profile layers,
/// mirroring `stack_lists`: a bare `-*` clears the accumulator, a `-token`
/// removes the matching prior token, and a plain token is appended once. The
/// `*` system prefix and `-` removal prefix are preserved in the result.
fn stack_packages(layers: &[&str]) -> Vec<String> {
    let mut acc: Vec<String> = Vec::new();
    for layer in layers {
        for raw in layer.lines() {
            let token = raw.trim();
            if token.is_empty() || token.starts_with('#') {
                continue;
            }
            if token == "-*" {
                acc.clear();
            } else if let Some(rest) = token.strip_prefix('-') {
                acc.retain(|t| t != rest);
            } else if !acc.iter().any(|t| t == token) {
                acc.push(token.to_owned());
            }
        }
    }
    acc
}

/// Materialize `@system` from stacked `packages` files: the `*`-prefixed
/// survivors with the `*` stripped, matching `PackagesSystemSet.load`.
pub fn system_set(layers: &[&str]) -> Vec<String> {
    stack_packages(layers)
        .into_iter()
        .filter_map(|t| t.strip_prefix('*').map(str::to_owned))
        .collect()
}

/// Materialize `@profile` from stacked `packages` files: the non-`*` survivors.
/// Only meaningful under the `profile-set` profile format.
pub fn profile_set(layers: &[&str]) -> Vec<String> {
    stack_packages(layers)
        .into_iter()
        .filter(|t| !t.starts_with('*'))
        .collect()
}

/// Read `@selected` from the world file contents (one atom per non-empty line).
pub fn selected_set(world_file: &str) -> Vec<String> {
    world_file
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Read `@selected-sets` from the `world_sets` file: the `@name` set references
/// the operator has selected, one per non-empty line. A line without the `@`
/// prefix is normalized to `@name`, mirroring `WorldSelectedSetsSet`.
pub fn selected_sets(world_sets_file: &str) -> Vec<String> {
    world_sets_file
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            if l.starts_with('@') {
                l.to_owned()
            } else {
                format!("@{l}")
            }
        })
        .collect()
}

/// Compose `@world` as the union of `@profile`, `@selected`, `@system`, and the
/// resolved members of the operator's `world_sets`, de-duplicating while
/// preserving first-seen order. This mirrors the default `[world]` definition
/// (`@profile @selected @system`) and `WorldSelectedSet`'s union of the world
/// packages and world sets files.
pub fn world_set(
    profile: &[String],
    selected: &[String],
    system: &[String],
    set_members: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for group in [profile, selected, system, set_members] {
        for member in group {
            if !out.iter().any(|x| x == member) {
                out.push(member.clone());
            }
        }
    }
    out
}

/// Build the `@preserved-rebuild` set: installed packages that link a soname now
/// satisfied only by a preserved library, minus the preserved libraries' own
/// owners.
///
/// `consumers` is each installed `(cp, required (bucket, soname) pairs)`;
/// `preserved_sonames` are the `(bucket, soname)` pairs whose only remaining
/// provider is a preserved library; `preserved_owners` are the `cp`s that own a
/// preserved library. A consumer requiring any preserved-only `(bucket, soname)`
/// is selected for rebuild, excluding the owners themselves, mirroring
/// `PreservedLibraryConsumerSet.load`. Matching is scoped to the multilib
/// category bucket so a 32-bit requirement is not satisfied by a 64-bit provider.
pub fn preserved_rebuild_set(
    consumers: &[(String, Vec<(String, String)>)],
    preserved_sonames: &BTreeSet<(String, String)>,
    preserved_owners: &BTreeSet<String>,
) -> Vec<String> {
    let mut out: Vec<String> = consumers
        .iter()
        .filter(|(cp, sonames)| {
            !preserved_owners.contains(cp) && sonames.iter().any(|s| preserved_sonames.contains(s))
        })
        .map(|(cp, _)| cp.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Resolve a file-backed user set by name from the set config search path.
/// Returns the set's atom lines, or [`ConfigError::UnknownSet`] if no matching
/// file exists.
pub fn resolve_user_set(name: &str, search_dirs: &[&Path]) -> Result<Vec<String>, ConfigError> {
    for dir in search_dirs {
        let path = dir.join(name);
        if path.is_file() {
            let content = std::fs::read_to_string(&path).map_err(|_| ConfigError::Io { path })?;
            return Ok(content
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_owned)
                .collect());
        }
    }
    Err(ConfigError::UnknownSet {
        name: name.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_takes_star_entries() {
        let layer = "*sys-apps/portage\napp-misc/notsystem\n";
        assert_eq!(system_set(&[layer]), vec!["sys-apps/portage"]);
    }

    #[test]
    fn negative_system_entry_removes() {
        assert_eq!(
            system_set(&["*sys-apps/foo", "-*sys-apps/foo"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn bare_star_dash_clears_accumulated_system() {
        // A bare `-*` clears every entry stacked so far.
        assert_eq!(
            system_set(&["*sys-apps/foo\n*sys-apps/bar", "-*\n*sys-apps/baz"]),
            vec!["sys-apps/baz"]
        );
    }

    #[test]
    fn profile_set_takes_non_star_entries() {
        let layer = "*sys-apps/portage\napp-misc/profileonly\n";
        assert_eq!(profile_set(&[layer]), vec!["app-misc/profileonly"]);
        assert_eq!(system_set(&[layer]), vec!["sys-apps/portage"]);
    }

    #[test]
    fn preserved_rebuild_selects_consumers_minus_owners() {
        let consumers = vec![
            (
                "app/uses-old".to_owned(),
                vec![("x86_64".to_owned(), "libold.so.1".to_owned())],
            ),
            (
                "app/unaffected".to_owned(),
                vec![("x86_64".to_owned(), "libcurrent.so.2".to_owned())],
            ),
            (
                "lib/owner".to_owned(),
                vec![("x86_64".to_owned(), "libold.so.1".to_owned())],
            ),
        ];
        let preserved: BTreeSet<(String, String)> =
            [("x86_64".to_owned(), "libold.so.1".to_owned())]
                .into_iter()
                .collect();
        let owners: BTreeSet<String> = ["lib/owner".to_owned()].into_iter().collect();

        let set = preserved_rebuild_set(&consumers, &preserved, &owners);
        // The consumer of the preserved soname is selected; the unaffected
        // package and the preserved-lib owner are excluded.
        assert_eq!(set, vec!["app/uses-old".to_owned()]);
    }

    #[test]
    fn preserved_rebuild_omits_other_abi_and_alternative_provider() {
        let consumers = vec![
            // Requires the soname in the 32-bit bucket only.
            (
                "app/abi32".to_owned(),
                vec![("x86_32".to_owned(), "libold.so.1".to_owned())],
            ),
            // Requires the preserved-only 64-bit soname: selected.
            (
                "app/uses-old".to_owned(),
                vec![("x86_64".to_owned(), "libold.so.1".to_owned())],
            ),
        ];
        // Only the 64-bit soname is preserved-only; the 32-bit one has an
        // alternative provider and so is not in the preserved-only set.
        let preserved: BTreeSet<(String, String)> =
            [("x86_64".to_owned(), "libold.so.1".to_owned())]
                .into_iter()
                .collect();
        let owners: BTreeSet<String> = BTreeSet::new();

        let set = preserved_rebuild_set(&consumers, &preserved, &owners);
        // The 32-bit consumer is omitted (wrong bucket / alternative provider);
        // the 64-bit consumer is selected.
        assert_eq!(set, vec!["app/uses-old".to_owned()]);
    }

    #[test]
    fn world_unions_profile_selected_system_and_sets() {
        let profile = vec!["sys-libs/glibc".to_owned()];
        let selected = selected_set("dev-lang/rust\napp-editors/neovim\n");
        let system = vec!["sys-apps/portage".to_owned()];
        let set_members = vec!["dev-libs/from-set".to_owned(), "dev-lang/rust".to_owned()];
        let world = world_set(&profile, &selected, &system, &set_members);
        assert!(world.contains(&"sys-libs/glibc".to_owned()));
        assert!(world.contains(&"dev-lang/rust".to_owned()));
        assert!(world.contains(&"sys-apps/portage".to_owned()));
        assert!(world.contains(&"dev-libs/from-set".to_owned()));
        // The duplicate `dev-lang/rust` from the set members appears once.
        assert_eq!(world.iter().filter(|w| *w == "dev-lang/rust").count(), 1);
    }

    #[test]
    fn selected_sets_reads_at_name_references() {
        assert_eq!(
            selected_sets("@security\nmyset\n# comment\n\n"),
            vec!["@security".to_owned(), "@myset".to_owned()]
        );
    }

    #[test]
    fn user_set_resolves_and_unknown_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("myset"), "dev-libs/a\ndev-libs/b\n").unwrap();
        let dirs: &[&Path] = &[dir.path()];
        assert_eq!(resolve_user_set("myset", dirs).unwrap().len(), 2);
        assert!(matches!(
            resolve_user_set("nope", dirs),
            Err(ConfigError::UnknownSet { .. })
        ));
    }
}
