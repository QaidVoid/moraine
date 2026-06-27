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
use moraine_config::sets::{
    profile_set, resolve_user_set, selected_set, selected_sets, system_set, world_set,
};
use moraine_repo::{RepoConfig, RepoSet, discover};
use thiserror::Error;
use tracing::instrument;

use crate::sets::SetSource;

/// The recognized `FEATURES` tokens, copied from Portage's `SUPPORTED_FEATURES`
/// (`lib/portage/const.py`). A token outside this set is "unknown" and is warned
/// about or filtered per `unknown-features-warn`/`unknown-features-filter`.
pub const SUPPORTED_FEATURES: &[&str] = &[
    "assume-digests",
    "binpkg-docompress",
    "binpkg-dostrip",
    "binpkg-ignore-signature",
    "binpkg-logs",
    "binpkg-multi-instance",
    "binpkg-request-signature",
    "binpkg-signing",
    "buildpkg",
    "buildpkg-live",
    "buildpkg-proactive",
    "buildsyspkg",
    "candy",
    "case-insensitive-fs",
    "ccache",
    "chflags",
    "clean-logs",
    "collision-protect",
    "compress-build-logs",
    "compress-index",
    "compressdebug",
    "config-protect-if-modified",
    "dedupdebug",
    "digest",
    "distcc",
    "distlocks",
    "downgrade-backup",
    "ebuild-locks",
    "fail-clean",
    "fakeroot",
    "fixlafiles",
    "force-mirror",
    "getbinpkg",
    "gpg-keepalive",
    "home-dir-template-copy",
    "icecream",
    "installsources",
    "ipc-sandbox",
    "jobserver-token",
    "keeptemp",
    "keepwork",
    "lmirror",
    "merge-sync",
    "merge-wait",
    "metadata-transfer",
    "mirror",
    "mount-sandbox",
    "multilib-strict",
    "network-sandbox",
    "network-sandbox-proxy",
    "news",
    "noauto",
    "noclean",
    "nodoc",
    "noinfo",
    "noman",
    "nostrip",
    "notitles",
    "packdebug",
    "parallel-fetch",
    "parallel-install",
    "pid-sandbox",
    "pkgdir-index-trusted",
    "prelink-checksums",
    "preserve-libs",
    "protect-owned",
    "python-trace",
    "qa-unresolved-soname-deps",
    "sandbox",
    "selinux",
    "sesandbox",
    "sfperms",
    "sign",
    "skiprocheck",
    "split-elog",
    "split-log",
    "splitdebug",
    "strict",
    "strict-keepdir",
    "stricter",
    "suidctl",
    "test",
    "test-fail-continue",
    "unknown-features-filter",
    "unknown-features-warn",
    "unmerge-backup",
    "unmerge-logs",
    "unmerge-orphans",
    "unprivileged",
    "userfetch",
    "userpriv",
    "usersandbox",
    "usersync",
    "warn-on-large-env",
    "webrsync-gpg",
    "xattr",
];

/// Resolve whether an incremental `FEATURES` token is enabled, honoring a later
/// `-token` negation; the final occurrence wins.
fn feature_enabled(raw: &[String], name: &str, default: bool) -> bool {
    let neg = format!("-{name}");
    let mut on = default;
    for token in raw {
        if token == name {
            on = true;
        } else if *token == neg {
            on = false;
        }
    }
    on
}

/// Validate `FEATURES` tokens against [`SUPPORTED_FEATURES`]. Unknown tokens are
/// warned about when `unknown-features-warn` is set (default on) and dropped from
/// the effective set when `unknown-features-filter` is set, matching Portage.
fn validate_features(raw: Vec<String>) -> Vec<String> {
    let warn = feature_enabled(&raw, "unknown-features-warn", true);
    let filter = feature_enabled(&raw, "unknown-features-filter", false);

    let is_known = |token: &str| {
        let name = token.strip_prefix('-').unwrap_or(token);
        name.is_empty() || SUPPORTED_FEATURES.contains(&name)
    };

    if warn {
        let unknown: Vec<&str> = raw
            .iter()
            .map(String::as_str)
            .filter(|t| !is_known(t))
            .collect();
        if !unknown.is_empty() {
            tracing::warn!(
                "FEATURES variable contains unknown value(s): {}",
                unknown.join(", ")
            );
        }
    }

    if filter {
        raw.into_iter().filter(|t| is_known(t)).collect()
    } else {
        raw
    }
}

/// Map the binary-package signature `FEATURES` to a [`SignaturePolicy`].
///
/// `binpkg-ignore-signature` relaxes verification, `binpkg-request-signature`
/// makes an unsigned Manifest fatal, and otherwise a present signature is
/// verified, matching Portage's defaults.
pub fn signature_policy(features: &[String]) -> moraine_binpkg::SignaturePolicy {
    use moraine_binpkg::SignaturePolicy;
    if feature_enabled(features, "binpkg-ignore-signature", false) {
        SignaturePolicy::IgnoreSignature
    } else if feature_enabled(features, "binpkg-request-signature", false) {
        SignaturePolicy::RequestSignature
    } else {
        SignaturePolicy::VerifyIfPresent
    }
}

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
    /// The `@profile` set members (only under the `profile-set` format).
    pub profile_set: Vec<String>,
    /// The `@world` set members.
    pub world: Vec<String>,
    /// The `@preserved-rebuild` set members, computed from the preserved-libs
    /// registry and installed soname data when that set is requested.
    pub preserved_rebuild: Vec<String>,
    /// The `/etc/portage/sets/` search directories for resolving named user sets.
    pub set_search_dirs: Vec<PathBuf>,
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
        if let Some(note) = profile.deprecation() {
            tracing::warn!("the selected profile is deprecated; replacement: {note}");
        }

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
        // The profile fixes PROFILE_ONLY_VARIABLES (for example `ARCH`); a user
        // `make.conf` or the inherited environment must not override them. The
        // stacked set is known after make.globals and the profile make.defaults.
        let profile_only: Vec<String> = env
            .get("PROFILE_ONLY_VARIABLES")
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let make_conf = config_dir.join("etc/portage/make.conf");
        if make_conf.exists() {
            // Capture the profile-fixed values, merge make.conf (so `$VAR`
            // expansion still sees the profile values), then restore the
            // profile-only variables, dropping any make.conf override.
            let saved: Vec<(String, Option<String>)> = profile_only
                .iter()
                .map(|key| (key.clone(), env.get(key).map(str::to_owned)))
                .collect();
            env.merge_path(&make_conf)
                .map_err(ConfigLoadError::MakeConf)?;
            for (key, value) in saved {
                match value {
                    Some(value) => env.set(key, value),
                    None => {
                        env.remove(&key);
                    }
                }
            }
        }
        // The known architecture keywords come from each repository's
        // `profiles/arch.list`, exported as PORTAGE_ARCHLIST.
        let archlist = arch_list(repos.as_ref());
        if !archlist.is_empty() {
            env.set("PORTAGE_ARCHLIST", archlist.join(" "));
        }
        let arch = env.get("ARCH").unwrap_or_default().to_owned();
        let tokens = |key: &str| {
            env.get(key)
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        let features = validate_features(tokens("FEATURES"));
        // When configuration leaves CONFIG_PROTECT unset, fall back to the
        // make.globals defaults so a minimal config root still protects `/etc`
        // and masks `/etc/env.d`.
        let mut config_protect = tokens("CONFIG_PROTECT");
        if config_protect.is_empty() {
            config_protect.push("/etc".to_owned());
        }
        let mut config_protect_mask = tokens("CONFIG_PROTECT_MASK");
        if config_protect_mask.is_empty() {
            config_protect_mask.push("/etc/env.d".to_owned());
        }

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

        // Under the profile-set format the non-`*` `packages` entries form the
        // `@profile` set, which is part of world selection.
        let profile_set_members = if profile_set_active(&profile, repos.as_ref()) {
            profile_set(&layer_refs)
        } else {
            Vec::new()
        };

        // The `world_sets` file selects `@name` set references; expand each from
        // the `/etc/portage/sets/` search path and union the members into world.
        let set_search_dirs = vec![config_dir.join("etc/portage/sets")];
        let world_sets_path = roots.root_dir().join("var/lib/portage/world_sets");
        let world_sets_contents = std::fs::read_to_string(&world_sets_path).unwrap_or_default();
        let dir_refs: Vec<&Path> = set_search_dirs.iter().map(PathBuf::as_path).collect();
        let mut world_set_members: Vec<String> = Vec::new();
        for set_ref in selected_sets(&world_sets_contents) {
            let name = set_ref.trim_start_matches('@');
            if let Ok(members) = resolve_user_set(name, &dir_refs) {
                for member in members {
                    if !world_set_members.contains(&member) {
                        world_set_members.push(member);
                    }
                }
            }
        }

        let world = world_set(&profile_set_members, &selected, &system, &world_set_members);

        Ok(ConfigContext {
            profile,
            vars: env,
            arch,
            features,
            config_protect,
            config_protect_mask,
            system,
            selected,
            profile_set: profile_set_members,
            world,
            preserved_rebuild: Vec::new(),
            set_search_dirs,
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

/// Whether the selected profile's owning repository declares the `profile-set`
/// format, under which the `packages` file's non-`*` entries are the `@profile`
/// set.
fn profile_set_active(profile: &ProfileStack, repos: Option<&RepoSet>) -> bool {
    let (Some(node), Some(set)) = (profile.nodes.last(), repos) else {
        return false;
    };
    owning_repo(set, &node.path)
        .map(|c| c.profile_formats.iter().any(|f| f == "profile-set"))
        .unwrap_or(false)
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

/// Collect the known architecture keywords from each repository's
/// `profiles/arch.list`, in repository order with duplicates removed.
fn arch_list(repos: Option<&RepoSet>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(set) = repos else {
        return out;
    };
    for repo in set.ordered() {
        let Ok(content) = std::fs::read_to_string(repo.location.join("profiles/arch.list")) else {
            continue;
        };
        for line in content.lines() {
            let arch = line.trim();
            if arch.is_empty() || arch.starts_with('#') {
                continue;
            }
            if !out.iter().any(|a| a == arch) {
                out.push(arch.to_owned());
            }
        }
    }
    out
}

/// Load and stack `profiles/thirdpartymirrors` across the discovered
/// repositories (masters first), mapping a named mirror group to its base URIs
/// so `mirror://group/path` resolves. Each line is `group uri1 uri2 ...`; URIs
/// for a repeated group are appended, mirroring `stack_dictlist`.
pub fn thirdparty_mirrors(repos: &RepoSet) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut groups: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for repo in repos.ordered() {
        let path = repo.location.join("profiles/thirdpartymirrors");
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(group) = parts.next() else {
                continue;
            };
            let entry = groups.entry(group.to_owned()).or_default();
            for uri in parts {
                if !entry.iter().any(|u| u == uri) {
                    entry.push(uri.to_owned());
                }
            }
        }
    }
    groups
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
            "profile" => Some(self.profile_set.clone()),
            "preserved-rebuild" => Some(self.preserved_rebuild.clone()),
            // Any other name resolves as a file-backed user set from the
            // `/etc/portage/sets/` search path; `None` only when no file exists.
            other => {
                let dirs: Vec<&Path> = self.set_search_dirs.iter().map(PathBuf::as_path).collect();
                resolve_user_set(other, &dirs).ok()
            }
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
    fn features_validation_warns_and_filters_unknown() {
        // Without the filter token, unknown values are retained (only warned).
        let kept = validate_features(vec![
            "ccache".into(),
            "bogus-feature".into(),
            "candy".into(),
        ]);
        assert!(kept.contains(&"bogus-feature".to_owned()));

        // With the filter token, unknown values are dropped; known ones stay.
        let filtered = validate_features(vec![
            "unknown-features-filter".into(),
            "ccache".into(),
            "bogus-feature".into(),
            "notitles".into(),
        ]);
        assert!(!filtered.contains(&"bogus-feature".to_owned()));
        assert!(filtered.contains(&"ccache".to_owned()));
        assert!(filtered.contains(&"notitles".to_owned()));
    }

    #[test]
    fn feature_negation_respects_last_occurrence() {
        assert!(!feature_enabled(
            &[
                "unknown-features-warn".into(),
                "-unknown-features-warn".into()
            ],
            "unknown-features-warn",
            true,
        ));
        assert!(feature_enabled(&[], "unknown-features-warn", true));
        assert!(feature_enabled(
            &["unknown-features-filter".into()],
            "unknown-features-filter",
            false,
        ));
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
        // A minimal config root still defaults to protecting `/etc` and masking
        // `/etc/env.d`.
        assert_eq!(ctx.config_protect, vec!["/etc".to_owned()]);
        assert_eq!(ctx.config_protect_mask, vec!["/etc/env.d".to_owned()]);
    }

    #[test]
    fn set_source_resolves_named_user_set() {
        let dir = tempfile::tempdir().unwrap();
        let sets_dir = dir.path().join("etc/portage/sets");
        std::fs::create_dir_all(&sets_dir).unwrap();
        std::fs::write(sets_dir.join("myset"), "dev-libs/a\ndev-libs/b\n").unwrap();
        let ctx = ConfigContext {
            profile: ProfileStack::default(),
            vars: VarMap::new(),
            arch: "amd64".to_owned(),
            features: Vec::new(),
            config_protect: Vec::new(),
            config_protect_mask: Vec::new(),
            system: Vec::new(),
            selected: Vec::new(),
            profile_set: Vec::new(),
            world: Vec::new(),
            preserved_rebuild: Vec::new(),
            set_search_dirs: vec![sets_dir],
        };
        assert_eq!(
            ctx.members("myset").unwrap(),
            vec!["dev-libs/a".to_owned(), "dev-libs/b".to_owned()]
        );
        assert!(ctx.members("nonexistent").is_none());
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
            profile_set: Vec::new(),
            world: vec!["app/editor".to_owned(), "sys-apps/baselayout".to_owned()],
            preserved_rebuild: Vec::new(),
            set_search_dirs: Vec::new(),
        };
        assert_eq!(ctx.members("system").unwrap(), ctx.system);
        assert_eq!(ctx.members("selected").unwrap(), ctx.selected);
        assert_eq!(ctx.members("world").unwrap(), ctx.world);
        assert!(ctx.members("nope").is_none());
    }

    #[test]
    fn make_conf_cannot_override_profile_only_arch() {
        let dir = tempfile::tempdir().unwrap();
        // A single-node profile fixes ARCH via PROFILE_ONLY_VARIABLES.
        let prof = dir.path().join("prof");
        std::fs::create_dir_all(&prof).unwrap();
        std::fs::write(prof.join("eapi"), "8\n").unwrap();
        std::fs::write(
            prof.join("make.defaults"),
            "PROFILE_ONLY_VARIABLES=\"ARCH\"\nARCH=\"amd64\"\n",
        )
        .unwrap();
        let portage = dir.path().join("etc/portage");
        std::fs::create_dir_all(&portage).unwrap();
        std::os::unix::fs::symlink(&prof, portage.join("make.profile")).unwrap();
        // make.conf tries to override the profile-fixed ARCH.
        std::fs::write(portage.join("make.conf"), "ARCH=\"x86\"\n").unwrap();
        let roots = Roots {
            root: Some(dir.path().to_path_buf()),
            config_root: Some(dir.path().to_path_buf()),
            profile: None,
        };
        let ctx = ConfigContext::load(&roots).unwrap();
        // The profile value wins; the make.conf assignment is dropped.
        assert_eq!(ctx.vars.get("ARCH"), Some("amd64"));
        assert_eq!(ctx.arch, "amd64");
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
    fn arch_list_loads_from_repo() {
        let dir = tempfile::tempdir().unwrap();
        let loc = dir.path().join("gentoo");
        std::fs::create_dir_all(loc.join("profiles")).unwrap();
        std::fs::write(loc.join("profiles/repo_name"), "gentoo\n").unwrap();
        std::fs::write(loc.join("profiles/arch.list"), "amd64\narm64\n# x86\n").unwrap();
        let conf = dir.path().join("repos.conf");
        std::fs::write(&conf, format!("[gentoo]\nlocation = {}\n", loc.display())).unwrap();
        let repos = discover(&conf).unwrap();
        assert_eq!(arch_list(Some(&repos)), vec!["amd64", "arm64"]);
    }

    #[test]
    fn thirdparty_mirrors_load_and_stack() {
        let dir = tempfile::tempdir().unwrap();
        let loc = dir.path().join("gentoo");
        std::fs::create_dir_all(loc.join("profiles")).unwrap();
        std::fs::write(loc.join("profiles/repo_name"), "gentoo\n").unwrap();
        std::fs::write(
            loc.join("profiles/thirdpartymirrors"),
            "gnu https://a/gnu https://b/gnu\nkernel https://k/\n",
        )
        .unwrap();
        let conf = dir.path().join("repos.conf");
        std::fs::write(&conf, format!("[gentoo]\nlocation = {}\n", loc.display())).unwrap();
        let repos = discover(&conf).unwrap();
        let groups = thirdparty_mirrors(&repos);
        assert_eq!(
            groups.get("gnu").unwrap(),
            &vec!["https://a/gnu".to_owned(), "https://b/gnu".to_owned()]
        );
        assert_eq!(
            groups.get("kernel").unwrap(),
            &vec!["https://k/".to_owned()]
        );
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
