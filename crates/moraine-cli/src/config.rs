//! Loading configuration and package sets for the binary.
//!
//! `moraine-config` exposes building blocks rather than a single loader, so this
//! module assembles a [`ConfigContext`] from the global root selection: it
//! resolves the profile stack, parses `make.conf`, materializes the standard
//! sets, and implements [`SetSource`] over them. The standard set contents come
//! from `moraine-config`, never from ad hoc file reads here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use miette::Diagnostic;
use moraine_config::makeconf::VarMap;
use moraine_config::profile::{ProfileContext, ProfileStack, read_profile_formats};
use moraine_config::sets::{selected_set, system_set, world_set};
use thiserror::Error;
use tracing::instrument;

use crate::sets::SetSource;

/// Global root and profile selection from the command line.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    /// The installed-tree root (`--root`), default `/`.
    pub root: Option<PathBuf>,
    /// The configuration root (`--config-root`), default `/`.
    pub config_root: Option<PathBuf>,
    /// An explicit profile directory (`--profile`).
    pub profile: Option<PathBuf>,
}

impl Roots {
    /// The effective installed-tree root.
    pub fn root_dir(&self) -> PathBuf {
        self.root.clone().unwrap_or_else(|| PathBuf::from("/"))
    }

    /// The effective configuration root.
    pub fn config_dir(&self) -> PathBuf {
        self.config_root
            .clone()
            .or_else(|| self.root.clone())
            .unwrap_or_else(|| PathBuf::from("/"))
    }
}

/// Errors from loading the configuration context.
#[derive(Debug, Error, Diagnostic)]
pub enum ConfigLoadError {
    /// The profile stack could not be resolved.
    #[error("failed to resolve the active profile")]
    #[diagnostic(code(moraine::config::profile))]
    Profile(#[source] moraine_config::ConfigError),

    /// `make.conf` could not be parsed.
    #[error("failed to parse make.conf")]
    #[diagnostic(code(moraine::config::makeconf))]
    MakeConf(#[source] moraine_config::ConfigError),
}

/// The loaded configuration and set context for one invocation.
#[derive(Debug, Clone)]
pub struct ConfigContext {
    /// The active profile stack.
    pub profile: ProfileStack,
    /// The system architecture keyword.
    pub arch: String,
    /// The `FEATURES` tokens from configuration.
    pub features: Vec<String>,
    /// The `CONFIG_PROTECT` paths from configuration.
    pub config_protect: Vec<String>,
    /// The `CONFIG_PROTECT_MASK` paths from configuration.
    pub config_protect_mask: Vec<String>,
    /// The `@system` set members.
    pub system: Vec<String>,
    /// The `@selected` set members (world file contents).
    pub selected: Vec<String>,
    /// The `@world` set members.
    pub world: Vec<String>,
}

impl ConfigContext {
    /// Load the context from the given roots.
    ///
    /// Missing files are tolerated: an absent world file yields an empty
    /// `@selected`, and an unresolvable profile yields an empty stack so the
    /// read-only path still runs against whatever data is present.
    #[instrument(skip(roots))]
    pub fn load(roots: &Roots) -> Result<ConfigContext, ConfigLoadError> {
        let config_dir = roots.config_dir();
        let profile = load_profile(&config_dir, roots.profile.as_deref());

        let mut env = VarMap::new();
        if let Ok(defaults) = profile.make_defaults() {
            for (key, value) in defaults.iter() {
                env.set(key.clone(), value.clone());
            }
        }
        let make_conf = config_dir.join("etc/portage/make.conf");
        if make_conf.exists() {
            env.merge_path(&make_conf)
                .map_err(ConfigLoadError::MakeConf)?;
        }
        let arch = env.get("ARCH").unwrap_or_default().to_owned();
        let tokens = |key: &str| {
            env.get(key)
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let features = tokens("FEATURES");
        let config_protect = tokens("CONFIG_PROTECT");
        let config_protect_mask = tokens("CONFIG_PROTECT_MASK");

        let profile_layers: Vec<String> = profile
            .nodes
            .iter()
            .filter_map(|node| std::fs::read_to_string(node.path.join("packages")).ok())
            .collect();
        let layer_refs: Vec<&str> = profile_layers.iter().map(String::as_str).collect();
        let system = system_set(&layer_refs);

        let world_path = roots.root_dir().join("var/lib/portage/world");
        let world_contents = std::fs::read_to_string(&world_path).unwrap_or_default();
        let selected = selected_set(&world_contents);

        let world = world_set(&selected, &system);

        Ok(ConfigContext {
            profile,
            arch,
            features,
            config_protect,
            config_protect_mask,
            system,
            selected,
            world,
        })
    }
}

/// Resolve the profile stack, tolerating an unresolvable profile.
fn load_profile(config_dir: &Path, explicit: Option<&Path>) -> ProfileStack {
    let formats: Vec<String> = explicit.map(read_profile_formats).unwrap_or_default();
    let ctx = ProfileContext {
        repo_profiles: &|_| None,
        formats: &formats,
    };
    if let Some(profile) = explicit {
        return ProfileStack::from_profile(profile, &ctx).unwrap_or_default();
    }
    ProfileStack::resolve_active(config_dir, &ctx).unwrap_or_default()
}

impl SetSource for ConfigContext {
    fn members(&self, name: &str) -> Option<Vec<String>> {
        match name {
            "world" => Some(self.world.clone()),
            "system" => Some(self.system.clone()),
            "selected" => Some(self.selected.clone()),
            _ => None,
        }
    }
}

/// The installed `category/package` set, for news relevance.
///
/// Reads from the world and system sets as a stand-in until the installed-store
/// query is wired through the resolution path.
pub fn installed_set_heads(ctx: &ConfigContext) -> BTreeSet<String> {
    ctx.world
        .iter()
        .chain(ctx.system.iter())
        .map(|atom| atom_head(atom).to_owned())
        .collect()
}

/// The `category/package` head of an atom string.
fn atom_head(atom: &str) -> &str {
    let trimmed = atom.trim_start_matches(['>', '<', '=', '~', '!']);
    match trimmed.split_once(':') {
        Some((head, _)) => head,
        None => trimmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_default_to_filesystem_root() {
        let roots = Roots::default();
        assert_eq!(roots.root_dir(), PathBuf::from("/"));
        assert_eq!(roots.config_dir(), PathBuf::from("/"));
    }

    #[test]
    fn config_root_falls_back_to_root() {
        let roots = Roots {
            root: Some(PathBuf::from("/mnt/target")),
            config_root: None,
            profile: None,
        };
        assert_eq!(roots.config_dir(), PathBuf::from("/mnt/target"));
    }

    #[test]
    fn load_tolerates_empty_root() {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots {
            root: Some(dir.path().to_path_buf()),
            config_root: Some(dir.path().to_path_buf()),
            profile: None,
        };
        let ctx = ConfigContext::load(&roots).unwrap();
        assert!(ctx.world.is_empty());
        assert!(ctx.system.is_empty());
    }

    #[test]
    fn set_source_dispatches_standard_sets() {
        let ctx = ConfigContext {
            profile: ProfileStack::default(),
            arch: "amd64".to_owned(),
            features: Vec::new(),
            config_protect: Vec::new(),
            config_protect_mask: Vec::new(),
            system: vec!["sys-apps/baselayout".to_owned()],
            selected: vec!["app/editor".to_owned()],
            world: vec!["app/editor".to_owned(), "sys-apps/baselayout".to_owned()],
        };
        assert_eq!(ctx.members("system").unwrap(), ctx.system);
        assert_eq!(ctx.members("selected").unwrap(), ctx.selected);
        assert_eq!(ctx.members("world").unwrap(), ctx.world);
        assert!(ctx.members("nope").is_none());
    }

    #[test]
    fn selected_reads_world_file() {
        let dir = tempfile::tempdir().unwrap();
        let world = dir.path().join("var/lib/portage/world");
        std::fs::create_dir_all(world.parent().unwrap()).unwrap();
        std::fs::write(&world, "app/editor\ndev-libs/openssl\n").unwrap();
        let roots = Roots {
            root: Some(dir.path().to_path_buf()),
            config_root: Some(dir.path().to_path_buf()),
            profile: None,
        };
        let ctx = ConfigContext::load(&roots).unwrap();
        assert_eq!(
            ctx.selected,
            vec!["app/editor".to_owned(), "dev-libs/openssl".to_owned()]
        );
    }
}
