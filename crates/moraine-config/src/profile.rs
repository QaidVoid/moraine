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

/// Context for resolving profile parent references.
pub struct ProfileContext<'a> {
    /// Maps a repository name to its `profiles` directory. The empty name is
    /// the current repository, used for leading-colon parents.
    pub repo_profiles: &'a dyn Fn(&str) -> Option<PathBuf>,
    /// The active repository's `profile-formats`, controlling which parent
    /// forms are permitted.
    pub formats: &'a [String],
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
        for raw in content.lines() {
            let reference = raw.trim();
            if reference.is_empty() || reference.starts_with('#') {
                continue;
            }
            let parent_dir = resolve_parent(reference, dir, ctx, &parent_file)?;
            add_node(&parent_dir, ctx, nodes, visited, false)?;
        }
    }

    let eapi = read_eapi(dir);
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
        // Leading-colon form, relative to the current repository's profiles.
        if !ctx.formats.iter().any(|f| f == "portage-2") {
            return Err(err(
                "leading-colon parents require the portage-2 profile format",
            ));
        }
        let root = (ctx.repo_profiles)("").ok_or_else(|| err("current repository unknown"))?;
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

fn read_eapi(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("eapi"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "0".to_owned())
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
            formats: &[],
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
            formats: &[],
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

        let ctx_no = ProfileContext {
            repo_profiles: &|_| Some(PathBuf::from("/nonexistent")),
            formats: &[],
        };
        assert!(ProfileStack::from_profile(&child, &ctx_no).is_err());
    }

    #[test]
    fn unsupported_eapi_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let node = dir.path().join("p");
        fs::create_dir_all(&node).unwrap();
        write(&node.join("eapi"), "banana\n");
        let ctx = ProfileContext {
            repo_profiles: &|_| None,
            formats: &[],
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
            formats: &[],
        };
        let stack = ProfileStack::from_profile(&child, &ctx).unwrap();
        let defaults = stack.make_defaults().unwrap();
        assert_eq!(defaults.get("FOO"), Some("child"));
        assert_eq!(defaults.get("ARCH"), Some("amd64"));
    }
}
