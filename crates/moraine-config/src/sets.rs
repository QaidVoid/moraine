//! Package sets: `@system`, `@selected`, `@world`, and file-backed user sets.

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

/// Compose `@world` as `@selected` plus `@system`.
pub fn world_set(selected: &[String], system: &[String]) -> Vec<String> {
    let mut out = selected.to_vec();
    for member in system {
        if !out.iter().any(|x| x == member) {
            out.push(member.clone());
        }
    }
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
    fn world_includes_system() {
        let selected = selected_set("dev-lang/rust\napp-editors/neovim\n");
        let system = vec!["sys-apps/portage".to_owned()];
        let world = world_set(&selected, &system);
        assert!(world.contains(&"dev-lang/rust".to_owned()));
        assert!(world.contains(&"sys-apps/portage".to_owned()));
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
