//! Profile stack construction.
//!
//! Profiles cascade through `parent` files: each node's parents are stacked
//! before the node itself (depth-first, parents before children), so later
//! nodes override earlier ones. The active profile comes from
//! `/etc/portage/make.profile`, and `/etc/portage/profile` is appended last as
//! the user node.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::error::ConfigError;
use crate::makeconf::VarMap;

/// A single node in a resolved profile stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileNode {
    /// The node's directory.
    pub path: PathBuf,
    /// The node's EAPI (defaulting to `"0"`).
    pub eapi: String,
    /// Whether this is the user-config node (`/etc/portage/profile`).
    pub is_user: bool,
}

/// The owning repository's profile metadata for one profile node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoProfileInfo {
    /// The owning repository's `profiles` directory, used to resolve a
    /// leading-colon parent reference relative to the same repository.
    pub profiles_dir: Option<PathBuf>,
    /// The owning repository's `profile-formats`, controlling which parent
    /// forms are permitted for this node.
    pub formats: Vec<String>,
    /// The owning repository's default profile EAPI, applied when the node
    /// declares no `eapi` file.
    pub default_eapi: Option<String>,
}

/// Context for resolving profile parent references.
pub struct ProfileContext<'a> {
    /// Maps a repository name to its `profiles` directory, used for `repo:path`
    /// parents.
    pub repo_profiles: &'a dyn Fn(&str) -> Option<PathBuf>,
    /// Resolves the owning repository's profile metadata for a node directory,
    /// so `profile-formats` and the default EAPI are sourced per node from the
    /// repository that owns it rather than from the profile directory itself.
    pub node_repo: &'a dyn Fn(&Path) -> RepoProfileInfo,
}

/// An ordered profile stack, parents before children.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileStack {
    /// The ordered nodes.
    pub nodes: Vec<ProfileNode>,
}

impl ProfileStack {
    /// Build a stack rooted at `start`, walking `parent` files depth-first.
    pub fn from_profile(
        start: &Path,
        ctx: &ProfileContext<'_>,
    ) -> Result<ProfileStack, ConfigError> {
        if !start.is_dir() {
            return Err(ConfigError::ProfileParent {
                path: start.to_path_buf(),
                reference: String::new(),
                reason: "profile directory does not exist",
            });
        }
        let mut nodes = Vec::new();
        let mut visited = BTreeSet::new();
        add_node(start, ctx, &mut nodes, &mut visited, false)?;
        Ok(ProfileStack { nodes })
    }

    /// Resolve the active profile from `config_root`, then append the user
    /// profile node if present.
    pub fn resolve_active(
        config_root: &Path,
        ctx: &ProfileContext<'_>,
    ) -> Result<ProfileStack, ConfigError> {
        let primary = config_root.join("etc/portage/make.profile");
        let deprecated = config_root.join("etc/make.profile");
        let target = if primary.exists() {
            primary
        } else if deprecated.exists() {
            deprecated
        } else {
            return Err(ConfigError::Io { path: primary });
        };
        let resolved = std::fs::canonicalize(&target).map_err(|_| ConfigError::Io {
            path: target.clone(),
        })?;
        let mut stack = ProfileStack::from_profile(&resolved, ctx)?;

        let user = config_root.join("etc/portage/profile");
        if user.is_dir() {
            stack.nodes.push(ProfileNode {
                eapi: read_eapi(&user),
                path: user,
                is_user: true,
            });
        }
        Ok(stack)
    }

    /// The deprecation notice from the selected (deepest non-user) profile node,
    /// if it ships a `deprecated` file. The content's first line names the
    /// replacement profile, mirroring `deprecated_profile_check`.
    pub fn deprecation(&self) -> Option<String> {
        let node = self.nodes.iter().rev().find(|n| !n.is_user)?;
        std::fs::read_to_string(node.path.join("deprecated"))
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    /// Merge each node's `make.defaults` in stack order.
    pub fn make_defaults(&self) -> Result<VarMap, ConfigError> {
        let mut vars = VarMap::new();
        for node in &self.nodes {
            let path = node.path.join("make.defaults");
            if path.is_file() {
                vars.merge_path(&path)?;
            }
        }
        Ok(vars)
    }
}

fn add_node(
    dir: &Path,
    ctx: &ProfileContext<'_>,
    nodes: &mut Vec<ProfileNode>,
    visited: &mut BTreeSet<PathBuf>,
    is_user: bool,
) -> Result<(), ConfigError> {
    let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if visited.contains(&canonical) {
        return Ok(());
    }
    visited.insert(canonical.clone());

    let parent_file = dir.join("parent");
    if parent_file.is_file() {
        let content = std::fs::read_to_string(&parent_file).map_err(|_| ConfigError::Io {
            path: parent_file.clone(),
        })?;
        let mut seen_reference = false;
        for raw in content.lines() {
            let reference = raw.trim();
            if reference.is_empty() || reference.starts_with('#') {
                continue;
            }
            seen_reference = true;
            let parent_dir = resolve_parent(reference, dir, ctx, &parent_file)?;
            if !parent_dir.is_dir() {
                return Err(ConfigError::ProfileParent {
                    path: parent_file.clone(),
                    reference: reference.to_owned(),
                    reason: "parent profile directory does not exist",
                });
            }
            add_node(&parent_dir, ctx, nodes, visited, false)?;
        }
        // A `parent` file that exists but lists no parents is a malformed
        // profile, matching Portage's `Empty parent file` error.
        if !seen_reference {
            return Err(ConfigError::ProfileParent {
                path: parent_file.clone(),
                reference: String::new(),
                reason: "empty parent file",
            });
        }
    }

    let eapi = node_eapi(dir, ctx);
    if moraine_eapi::level(&eapi).is_none() {
        return Err(ConfigError::UnsupportedEapi {
            path: dir.to_path_buf(),
            eapi,
        });
    }
    nodes.push(ProfileNode {
        path: dir.to_path_buf(),
        eapi,
        is_user,
    });
    Ok(())
}

fn resolve_parent(
    reference: &str,
    node_dir: &Path,
    ctx: &ProfileContext<'_>,
    parent_file: &Path,
) -> Result<PathBuf, ConfigError> {
    let err = |reason: &'static str| ConfigError::ProfileParent {
        path: parent_file.to_path_buf(),
        reference: reference.to_owned(),
        reason,
    };

    if let Some(rest) = reference.strip_prefix(':') {
        // Leading-colon form, relative to the repository that owns this node.
        let info = (ctx.node_repo)(node_dir);
        if !info.formats.iter().any(|f| f == "portage-2") {
            return Err(err(
                "leading-colon parents require the portage-2 profile format",
            ));
        }
        let root = info
            .profiles_dir
            .ok_or_else(|| err("current repository unknown"))?;
        return Ok(root.join(rest));
    }

    if let Some((repo, rel)) = reference.split_once(':') {
        let root = (ctx.repo_profiles)(repo).ok_or_else(|| err("unknown repository"))?;
        return Ok(root.join(rel));
    }

    let path = Path::new(reference);
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(node_dir.join(reference))
    }
}

/// Read a single space-separated key from a repository's `metadata/layout.conf`
/// directly, used when no parsed repository object is available.
fn read_layout_key(repo_root: &Path, key: &str) -> Vec<String> {
    let path = repo_root.join("metadata/layout.conf");
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(key)
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            return value.split_whitespace().map(str::to_owned).collect();
        }
    }
    Vec::new()
}

/// Read `profile-formats` from a repository's `layout.conf`.
pub fn read_profile_formats(repo_root: &Path) -> Vec<String> {
    read_layout_key(repo_root, "profile-formats")
}

/// Read the `masters` order from a repository's `layout.conf`.
pub fn read_masters(repo_root: &Path) -> Vec<String> {
    read_layout_key(repo_root, "masters")
}

/// The declared EAPI from a node's `eapi` file, if present and non-empty.
fn declared_eapi(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("eapi"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// A profile node's EAPI: its `eapi` file, else the owning repository's default
/// profile EAPI, else `0`.
fn node_eapi(dir: &Path, ctx: &ProfileContext<'_>) -> String {
    declared_eapi(dir)
        .or_else(|| (ctx.node_repo)(dir).default_eapi)
        .unwrap_or_else(|| "0".to_owned())
}

fn read_eapi(dir: &Path) -> String {
    declared_eapi(dir).unwrap_or_else(|| "0".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn parents_precede_children() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let child = dir.path().join("child");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&child).unwrap();
        write(&child.join("parent"), "../base\n");

        let ctx = ProfileContext {
            repo_profiles: &|_| None,
            node_repo: &|_| RepoProfileInfo::default(),
        };
        let stack = ProfileStack::from_profile(&child, &ctx).unwrap();
        assert_eq!(stack.nodes.len(), 2);
        assert!(stack.nodes[0].path.ends_with("base"));
        assert!(stack.nodes[1].path.ends_with("child"));
    }

    #[test]
    fn repo_path_parent_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let repo_profiles = dir.path().join("repo/profiles");
        let base = repo_profiles.join("base");
        fs::create_dir_all(&base).unwrap();
        let child = dir.path().join("child");
        fs::create_dir_all(&child).unwrap();
        write(&child.join("parent"), "gentoo:base\n");

        let rp = repo_profiles.clone();
        let ctx = ProfileContext {
            repo_profiles: &|name| {
                if name == "gentoo" {
                    Some(rp.clone())
                } else {
                    None
                }
            },
            node_repo: &|_| RepoProfileInfo::default(),
        };
        let stack = ProfileStack::from_profile(&child, &ctx).unwrap();
        assert!(stack.nodes[0].path.ends_with("base"));
    }

    #[test]
    fn leading_colon_gated_by_format() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        fs::create_dir_all(&child).unwrap();
        write(&child.join("parent"), ":base\n");

        // Without the portage-2 format, a leading-colon parent is rejected.
        let ctx_no = ProfileContext {
            repo_profiles: &|_| None,
            node_repo: &|_| RepoProfileInfo::default(),
        };
        assert!(ProfileStack::from_profile(&child, &ctx_no).is_err());
    }

    #[test]
    fn leading_colon_resolves_against_owning_repo() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = dir.path().join("repo/profiles");
        fs::create_dir_all(profiles.join("base")).unwrap();
        let child = dir.path().join("child");
        fs::create_dir_all(&child).unwrap();
        write(&child.join("parent"), ":base\n");

        let profiles_dir = profiles.clone();
        let ctx = ProfileContext {
            repo_profiles: &|_| None,
            node_repo: &|_| RepoProfileInfo {
                profiles_dir: Some(profiles_dir.clone()),
                formats: vec!["portage-2".to_owned()],
                default_eapi: None,
            },
        };
        let stack = ProfileStack::from_profile(&child, &ctx).unwrap();
        assert!(stack.nodes[0].path.ends_with("base"));
    }

    #[test]
    fn default_eapi_comes_from_owning_repo() {
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("p");
        fs::create_dir_all(&node).unwrap();
        // No `eapi` file: the owning repository's default applies.
        let ctx = ProfileContext {
            repo_profiles: &|_| None,
            node_repo: &|_| RepoProfileInfo {
                profiles_dir: None,
                formats: Vec::new(),
                default_eapi: Some("7".to_owned()),
            },
        };
        let stack = ProfileStack::from_profile(&node, &ctx).unwrap();
        assert_eq!(stack.nodes[0].eapi, "7");
    }

    #[test]
    fn empty_parent_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        fs::create_dir_all(&child).unwrap();
        // A parent file with only comments and blank lines names no parent.
        write(&child.join("parent"), "# just a comment\n\n");
        let ctx = ProfileContext {
            repo_profiles: &|_| None,
            node_repo: &|_| RepoProfileInfo::default(),
        };
        assert!(matches!(
            ProfileStack::from_profile(&child, &ctx),
            Err(ConfigError::ProfileParent {
                reason: "empty parent file",
                ..
            })
        ));
    }

    #[test]
    fn dangling_parent_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        fs::create_dir_all(&child).unwrap();
        write(&child.join("parent"), "../missing\n");
        let ctx = ProfileContext {
            repo_profiles: &|_| None,
            node_repo: &|_| RepoProfileInfo::default(),
        };
        assert!(matches!(
            ProfileStack::from_profile(&child, &ctx),
            Err(ConfigError::ProfileParent {
                reason: "parent profile directory does not exist",
                ..
            })
        ));
    }

    #[test]
    fn nonexistent_profile_dir_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ProfileContext {
            repo_profiles: &|_| None,
            node_repo: &|_| RepoProfileInfo::default(),
        };
        assert!(matches!(
            ProfileStack::from_profile(&dir.path().join("nope"), &ctx),
            Err(ConfigError::ProfileParent {
                reason: "profile directory does not exist",
                ..
            })
        ));
    }

    #[test]
    fn unsupported_eapi_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("p");
        fs::create_dir_all(&node).unwrap();
        write(&node.join("eapi"), "banana\n");
        let ctx = ProfileContext {
            repo_profiles: &|_| None,
            node_repo: &|_| RepoProfileInfo::default(),
        };
        assert!(matches!(
            ProfileStack::from_profile(&node, &ctx),
            Err(ConfigError::UnsupportedEapi { .. })
        ));
    }

    #[test]
    fn layout_conf_keys_parse() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("metadata/layout.conf"),
            "masters = gentoo\nprofile-formats = portage-2 profile-set\n",
        );
        assert_eq!(
            read_profile_formats(dir.path()),
            vec!["portage-2", "profile-set"]
        );
        assert_eq!(read_masters(dir.path()), vec!["gentoo"]);
    }

    #[test]
    fn make_defaults_child_overrides_parent() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("base");
        let child = dir.path().join("child");
        fs::create_dir_all(&base).unwrap();
        fs::create_dir_all(&child).unwrap();
        write(&child.join("parent"), "../base\n");
        write(
            &base.join("make.defaults"),
            "ARCH=\"amd64\"\nFOO=\"base\"\n",
        );
        write(&child.join("make.defaults"), "FOO=\"child\"\n");

        let ctx = ProfileContext {
            repo_profiles: &|_| None,
            node_repo: &|_| RepoProfileInfo::default(),
        };
        let stack = ProfileStack::from_profile(&child, &ctx).unwrap();
        let defaults = stack.make_defaults().unwrap();
        assert_eq!(defaults.get("FOO"), Some("child"));
        assert_eq!(defaults.get("ARCH"), Some("amd64"));
    }
}
