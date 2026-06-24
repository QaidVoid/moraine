//! Package sets: `@system`, `@selected`, `@world`, and file-backed user sets.

use std::path::Path;

use crate::error::ConfigError;

/// Materialize `@system` from stacked `packages` files. Only `*`-prefixed
/// entries are system members; a `-*entry` removes an inherited member.
pub fn system_set(layers: &[&str]) -> Vec<String> {
    let mut acc: Vec<String> = Vec::new();
    for layer in layers {
        for raw in layer.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("-*") {
                acc.retain(|x| x != rest);
            } else if let Some(rest) = line.strip_prefix('*')
                && !acc.iter().any(|x| x == rest)
            {
                acc.push(rest.to_owned());
            }
        }
    }
    acc
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
