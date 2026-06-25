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
use moraine_config::profile::{ProfileContext, ProfileStack, RepoProfileInfo};
use moraine_config::sets::{selected_set, system_set, world_set};
use moraine_repo::{RepoConfig, RepoSet, discover};
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
    /// The merged `make.defaults` plus `make.conf` variables.
    pub vars: VarMap,
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
        // Discover repositories so profile parents (`repo:path` and leading-colon
        // `:path`) resolve and per-node `profile-formats` and default EAPI come
        // from the owning repository. A missing or unreadable repos.conf is
        // tolerated as no repositories.
        let repos_conf = config_dir.join("etc/portage/repos.conf");
        let repos = if repos_conf.exists() {
            discover(&repos_conf).ok()
        } else {
            None
        };
        let profile = load_profile(&config_dir, roots.profile.as_deref(), repos.as_ref())?;

        let mut env = VarMap::new();
        // make.globals is the lowest configuration layer, below profile
        // make.defaults and make.conf. Its absence is tolerated.
        let make_globals = config_dir.join("usr/share/portage/config/make.globals");
        if make_globals.exists() {
            env.merge_path(&make_globals)
                .map_err(ConfigLoadError::MakeConf)?;
        }
        if let Ok(defaults) = profile.make_defaults() {
            for (key, value) in defaults.iter() {
                env.merge_var(key, value);
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
            vars: env,
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

/// Resolve the profile stack against the discovered repositories.
///
/// A missing profile selection (no `make.profile`) is tolerated as an empty
/// stack so the read-only path still runs, but a malformed parent chain or an
/// unsupported EAPI surfaces as an error rather than silently zeroing the stack.
fn load_profile(
    config_dir: &Path,
    explicit: Option<&Path>,
    repos: Option<&RepoSet>,
) -> Result<ProfileStack, ConfigLoadError> {
    let repo_profiles =
        |name: &str| -> Option<PathBuf> { repos?.get(name).map(|c| c.location.join("profiles")) };
    let node_repo = |path: &Path| -> RepoProfileInfo {
        let Some(set) = repos else {
            return RepoProfileInfo::default();
        };
        match owning_repo(set, path) {
            Some(c) => RepoProfileInfo {
                profiles_dir: Some(c.location.join("profiles")),
                formats: c.profile_formats.clone(),
                default_eapi: repo_default_eapi(&c.location),
            },
            None => RepoProfileInfo::default(),
        }
    };
    let ctx = ProfileContext {
        repo_profiles: &repo_profiles,
        node_repo: &node_repo,
    };

    if let Some(profile) = explicit {
        return ProfileStack::from_profile(profile, &ctx).map_err(ConfigLoadError::Profile);
    }
    match ProfileStack::resolve_active(config_dir, &ctx) {
        Ok(stack) => Ok(stack),
        // No selected profile (absent or unresolvable make.profile) stays
        // tolerant so a bare root still runs read-only.
        Err(moraine_config::ConfigError::Io { .. }) => Ok(ProfileStack::default()),
        Err(other) => Err(ConfigLoadError::Profile(other)),
    }
}

/// The repository whose `profiles` directory is the longest ancestor of `path`,
/// canonicalizing both sides so symlinked profiles and `..` components match.
fn owning_repo<'a>(set: &'a RepoSet, path: &Path) -> Option<&'a RepoConfig> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut best: Option<&RepoConfig> = None;
    let mut best_len = 0;
    for cfg in set.ordered() {
        let profiles = cfg.location.join("profiles");
        let profiles = std::fs::canonicalize(&profiles).unwrap_or(profiles);
        if path.starts_with(&profiles) {
            let len = profiles.as_os_str().len();
            if len >= best_len {
                best = Some(cfg);
                best_len = len;
            }
        }
    }
    best
}

/// Build the per-repository masking inputs for `resolve_config`: each
/// repository contributes its own `profiles/package.mask`, stacked over its
/// masters' (in resolved order) so a child repository inherits and may override
/// its masters, with every atom scoped to the repository.
pub fn repo_mask_inputs(repos: &RepoSet) -> Vec<moraine_config::RepoMaskInput> {
    repos
        .ordered()
        .map(|repo| {
            let mut profiles_dirs: Vec<PathBuf> = repos
                .order()
                .iter()
                .filter(|name| repo.masters.iter().any(|m| m == *name))
                .filter_map(|name| repos.get(name))
                .map(|m| m.location.join("profiles"))
                .collect();
            profiles_dirs.push(repo.location.join("profiles"));
            moraine_config::RepoMaskInput {
                name: repo.name.clone(),
                eapi: repo_default_eapi(&repo.location),
                profiles_dirs,
            }
        })
        .collect()
}

/// A repository's default profile EAPI: `layout.conf`'s
/// `profile_eapi_when_unspecified`, else the `profiles/eapi` file, else none.
fn repo_default_eapi(location: &Path) -> Option<String> {
    if let Ok(layout) = std::fs::read_to_string(location.join("metadata/layout.conf")) {
        for line in layout.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("profile_eapi_when_unspecified")
                && let Some(value) = rest.trim_start().strip_prefix('=')
            {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_owned());
                }
            }
        }
    }
    std::fs::read_to_string(location.join("profiles/eapi"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
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
            vars: VarMap::new(),
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
    fn make_globals_is_lowest_layer() {
        let dir = tempfile::tempdir().unwrap();
        let globals = dir.path().join("usr/share/portage/config/make.globals");
        std::fs::create_dir_all(globals.parent().unwrap()).unwrap();
        std::fs::write(&globals, "USE=\"a b\"\n").unwrap();
        let make_conf = dir.path().join("etc/portage/make.conf");
        std::fs::create_dir_all(make_conf.parent().unwrap()).unwrap();
        std::fs::write(&make_conf, "USE=\"c\"\n").unwrap();
        let roots = Roots {
            root: Some(dir.path().to_path_buf()),
            config_root: Some(dir.path().to_path_buf()),
            profile: None,
        };
        let ctx = ConfigContext::load(&roots).unwrap();
        // make.globals supplies the base USE, make.conf stacks onto it.
        assert_eq!(ctx.vars.get("USE"), Some("a b c"));
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
