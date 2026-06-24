//! Repository discovery and ordering.
//!
//! Parses `repos.conf` (a single INI file or a directory of `*.conf`
//! fragments), then enriches each repository from its `profiles/repo_name` and
//! `metadata/layout.conf`. The `repos.conf` value wins over `layout.conf` for
//! `masters`, `aliases`, and `eclass-overrides`, matching stock Portage
//! precedence.
//!
//! Discovery resolves a single deterministic repository order in which every
//! master is searched before the repositories that inherit it, with `priority`
//! breaking ties, and rejects cycles in the masters graph. It also builds an
//! eclass search path per repository so the importer can resolve inherited
//! eclasses through masters with `eclass-overrides` applied.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use tracing::instrument;

use crate::error::DiscoveryError;

/// The configuration of a single discovered repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    /// The repository name (the `repos.conf` section name, or the canonical
    /// `profiles/repo_name` when that overrides it).
    pub name: String,
    /// The on-disk location of the repository tree.
    pub location: PathBuf,
    /// The resolved master repositories, in declared order.
    pub masters: Vec<String>,
    /// The `priority` value, defaulting to `0`.
    pub priority: i32,
    /// Alternate names for the repository.
    pub aliases: Vec<String>,
    /// Repositories whose eclasses override inherited ones, in order.
    pub eclass_overrides: Vec<String>,
    /// The declared cache formats from `layout.conf` (for example `md5-dict`).
    pub cache_formats: Vec<String>,
    /// The declared profile formats from `layout.conf`.
    pub profile_formats: Vec<String>,
    /// The raw `sync-*` keys read from `repos.conf`.
    pub sync: BTreeMap<String, String>,
}

impl RepoConfig {
    /// The repository's `metadata/md5-cache` directory.
    pub fn md5_cache_dir(&self) -> PathBuf {
        self.location.join("metadata/md5-cache")
    }

    /// The repository's `eclass` directory.
    pub fn eclass_dir(&self) -> PathBuf {
        self.location.join("eclass")
    }
}

/// The full set of discovered repositories with a resolved search order.
#[derive(Debug, Clone)]
pub struct RepoSet {
    repos: HashMap<String, RepoConfig>,
    /// Repository names in the resolved search order: masters first, then the
    /// repositories that inherit them, with `priority` breaking ties.
    order: Vec<String>,
}

impl RepoSet {
    /// The repositories in the resolved search order.
    pub fn ordered(&self) -> impl Iterator<Item = &RepoConfig> {
        self.order.iter().filter_map(move |n| self.repos.get(n))
    }

    /// The resolved repository names in search order.
    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// Look up a repository by name.
    pub fn get(&self, name: &str) -> Option<&RepoConfig> {
        self.repos.get(name)
    }

    /// The number of discovered repositories.
    pub fn len(&self) -> usize {
        self.repos.len()
    }

    /// Whether no repositories were discovered.
    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
    }

    /// Build the eclass search path for `repo`: the ordered list of repository
    /// locations to consult when resolving an eclass, with `eclass-overrides`
    /// taking precedence over the repository itself and its masters.
    ///
    /// The result lists repository `eclass` directories in priority order. An
    /// override repository's eclass wins over the repository's own; the
    /// repository's own wins over an inherited master's, following the resolved
    /// masters order.
    #[instrument(skip(self), fields(repo = repo))]
    pub fn eclass_search_path(&self, repo: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let Some(cfg) = self.repos.get(repo) else {
            return out;
        };

        let push =
            |name: &str, out: &mut Vec<PathBuf>, seen: &mut std::collections::HashSet<String>| {
                if seen.insert(name.to_owned())
                    && let Some(c) = self.repos.get(name)
                {
                    out.push(c.eclass_dir());
                }
            };

        // Overrides take precedence over everything.
        for over in &cfg.eclass_overrides {
            push(over, &mut out, &mut seen);
        }
        // The repository's own eclasses.
        push(repo, &mut out, &mut seen);
        // Masters in resolved order (a master is earlier in `order`).
        for name in &self.order {
            if cfg.masters.contains(name) {
                push(name, &mut out, &mut seen);
            }
        }
        out
    }
}

/// Discover repositories from a `repos.conf` path (file or directory).
///
/// Reads every section as a repository, enriches each from `repo_name` and
/// `layout.conf`, applies `repos.conf`-over-`layout.conf` precedence, then
/// resolves the deterministic search order.
#[instrument(skip_all, fields(path = %repos_conf.as_ref().display()))]
pub fn discover(repos_conf: impl AsRef<Path>) -> Result<RepoSet, DiscoveryError> {
    let sections = parse_repos_conf(repos_conf.as_ref())?;
    let mut repos: HashMap<String, RepoConfig> = HashMap::new();

    for (name, section) in sections {
        let location = section
            .get("location")
            .map(|s| PathBuf::from(s.trim()))
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| DiscoveryError::MissingLocation { repo: name.clone() })?;

        let conf_masters = section.get("masters").map(|s| split_ws(s));
        let conf_aliases = section.get("aliases").map(|s| split_ws(s));
        let conf_overrides = section.get("eclass-overrides").map(|s| split_ws(s));
        let priority = section
            .get("priority")
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let sync: BTreeMap<String, String> = section
            .iter()
            .filter(|(k, _)| k.starts_with("sync-"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let layout = read_layout_conf(&location);

        // repos.conf overrides layout.conf for masters, aliases, eclass-overrides.
        let masters = conf_masters.unwrap_or(layout.masters);
        let aliases = conf_aliases.unwrap_or(layout.aliases);
        let eclass_overrides = conf_overrides.unwrap_or(layout.eclass_overrides);

        // Canonical name from profiles/repo_name when present.
        let canonical = read_repo_name(&location).unwrap_or_else(|| name.clone());

        let cfg = RepoConfig {
            name: canonical.clone(),
            location,
            masters,
            priority,
            aliases,
            eclass_overrides,
            cache_formats: layout.cache_formats,
            profile_formats: layout.profile_formats,
            sync,
        };
        repos.insert(canonical, cfg);
    }

    let order = resolve_order(&repos)?;
    Ok(RepoSet { repos, order })
}

/// Resolve the deterministic repository search order via a topological sort over
/// the masters graph, with `priority` then name breaking ties, rejecting cycles.
#[instrument(skip_all)]
fn resolve_order(repos: &HashMap<String, RepoConfig>) -> Result<Vec<String>, DiscoveryError> {
    // Validate master references.
    for cfg in repos.values() {
        for m in &cfg.masters {
            if !repos.contains_key(m) {
                return Err(DiscoveryError::UnknownMaster {
                    repo: cfg.name.clone(),
                    master: m.clone(),
                });
            }
        }
    }

    // Kahn's algorithm with deterministic tie-breaking. Edge: master -> repo.
    let mut indegree: HashMap<&str, usize> = repos.keys().map(|k| (k.as_str(), 0)).collect();
    for cfg in repos.values() {
        // Each distinct master that the repo inherits is one incoming edge.
        let mut seen = std::collections::HashSet::new();
        for m in &cfg.masters {
            if seen.insert(m.as_str()) {
                *indegree.get_mut(cfg.name.as_str()).expect("repo present") += 1;
            }
        }
    }

    // Higher priority is searched first; ties broken by name for determinism.
    let sort_key = |name: &str| {
        let p = repos.get(name).map(|c| c.priority).unwrap_or(0);
        // Negate priority so that higher priority sorts earlier in a min-order.
        (-p, name.to_owned())
    };

    let mut ready: Vec<&str> = indegree
        .iter()
        .filter(|&(_, &d)| d == 0)
        .map(|(&n, _)| n)
        .collect();
    ready.sort_by_key(|n| sort_key(n));

    let mut order = Vec::with_capacity(repos.len());
    while let Some(next) = ready.first().copied() {
        ready.remove(0);
        order.push(next.to_owned());
        // Decrement indegree of repos that inherit `next`.
        let mut newly: Vec<&str> = Vec::new();
        for cfg in repos.values() {
            if cfg.masters.iter().any(|m| m == next) {
                let d = indegree.get_mut(cfg.name.as_str()).expect("repo present");
                *d = d.saturating_sub(1);
                if *d == 0 {
                    newly.push(cfg.name.as_str());
                }
            }
        }
        for n in newly {
            ready.push(n);
        }
        ready.sort_by_key(|n| sort_key(n));
    }

    if order.len() != repos.len() {
        let mut cycle: Vec<String> = repos
            .keys()
            .filter(|k| !order.contains(k))
            .cloned()
            .collect();
        cycle.sort();
        return Err(DiscoveryError::MastersCycle {
            repos: cycle.join(", "),
        });
    }

    Ok(order)
}

/// The subset of `layout.conf` keys discovery consumes.
#[derive(Debug, Default)]
struct Layout {
    masters: Vec<String>,
    aliases: Vec<String>,
    eclass_overrides: Vec<String>,
    cache_formats: Vec<String>,
    profile_formats: Vec<String>,
}

/// Read and parse `metadata/layout.conf`, returning defaults when absent.
fn read_layout_conf(location: &Path) -> Layout {
    let path = location.join("metadata/layout.conf");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Layout::default();
    };
    let mut layout = Layout::default();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value: Vec<String> = split_ws(value);
        match key {
            "masters" => layout.masters = value,
            "aliases" => layout.aliases = value,
            "eclass-overrides" => layout.eclass_overrides = value,
            "cache-formats" => layout.cache_formats = value,
            "profile-formats" => layout.profile_formats = value,
            _ => {}
        }
    }
    layout
}

/// Read the canonical repository name from `profiles/repo_name`.
fn read_repo_name(location: &Path) -> Option<String> {
    let content = std::fs::read_to_string(location.join("profiles/repo_name")).ok()?;
    content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_owned)
}

fn split_ws(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_owned).collect()
}

/// A parsed INI section: an ordered key map.
type Section = BTreeMap<String, String>;

/// Parse a `repos.conf` path into `name -> section` pairs. Accepts a single file
/// or a directory of `*.conf` fragments, merging all sections.
fn parse_repos_conf(path: &Path) -> Result<BTreeMap<String, Section>, DiscoveryError> {
    let mut out: BTreeMap<String, Section> = BTreeMap::new();
    if path.is_dir() {
        let mut fragments: Vec<PathBuf> = std::fs::read_dir(path)
            .map_err(|source| DiscoveryError::Read {
                path: path.to_path_buf(),
                source,
            })?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|e| e == "conf").unwrap_or(false))
            .collect();
        fragments.sort();
        for frag in fragments {
            parse_ini_file(&frag, &mut out)?;
        }
    } else {
        parse_ini_file(path, &mut out)?;
    }
    Ok(out)
}

/// Parse a single INI file, merging its sections into `out`.
fn parse_ini_file(path: &Path, out: &mut BTreeMap<String, Section>) -> Result<(), DiscoveryError> {
    let content = std::fs::read_to_string(path).map_err(|source| DiscoveryError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let mut current: Option<String> = None;
    for (idx, raw) in content.lines().enumerate() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            let name = rest.strip_suffix(']').ok_or(DiscoveryError::Ini {
                path: path.to_path_buf(),
                line: idx + 1,
                reason: "unterminated section header",
            })?;
            let name = name.trim().to_owned();
            if name.is_empty() {
                return Err(DiscoveryError::Ini {
                    path: path.to_path_buf(),
                    line: idx + 1,
                    reason: "empty section name",
                });
            }
            out.entry(name.clone()).or_default();
            current = Some(name);
        } else if let Some((key, value)) = line.split_once('=') {
            let section = current.as_ref().ok_or(DiscoveryError::Ini {
                path: path.to_path_buf(),
                line: idx + 1,
                reason: "key outside of a section",
            })?;
            out.entry(section.clone())
                .or_default()
                .insert(key.trim().to_owned(), value.trim().to_owned());
        } else {
            return Err(DiscoveryError::Ini {
                path: path.to_path_buf(),
                line: idx + 1,
                reason: "line is neither a section header nor a key=value pair",
            });
        }
    }
    Ok(())
}

/// Strip a trailing `#` comment that is not inside a value.
fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(0) => "",
        Some(idx) => &line[..idx],
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Create a minimal repository tree under `root/name` and return its path.
    fn make_repo(root: &Path, name: &str, layout: &str) -> PathBuf {
        let loc = root.join(name);
        fs::create_dir_all(loc.join("profiles")).unwrap();
        fs::create_dir_all(loc.join("metadata")).unwrap();
        fs::create_dir_all(loc.join("eclass")).unwrap();
        fs::write(loc.join("profiles/repo_name"), format!("{name}\n")).unwrap();
        if !layout.is_empty() {
            fs::write(loc.join("metadata/layout.conf"), layout).unwrap();
        }
        loc
    }

    #[test]
    fn section_defines_repository() {
        let tmp = TempDir::new().unwrap();
        let loc = make_repo(tmp.path(), "gentoo", "");
        let conf = tmp.path().join("repos.conf");
        fs::write(
            &conf,
            format!(
                "[gentoo]\nlocation = {}\npriority = 5\nsync-type = rsync\nsync-uri = rsync://x\n",
                loc.display()
            ),
        )
        .unwrap();

        let set = discover(&conf).unwrap();
        let cfg = set.get("gentoo").unwrap();
        assert_eq!(cfg.location, loc);
        assert_eq!(cfg.priority, 5);
        assert_eq!(cfg.sync.get("sync-type").map(String::as_str), Some("rsync"));
        assert_eq!(
            cfg.sync.get("sync-uri").map(String::as_str),
            Some("rsync://x")
        );
    }

    #[test]
    fn directory_of_fragments_is_merged() {
        let tmp = TempDir::new().unwrap();
        let a = make_repo(tmp.path(), "a", "");
        let b = make_repo(tmp.path(), "b", "");
        let dir = tmp.path().join("repos.conf");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("a.conf"),
            format!("[a]\nlocation = {}\n", a.display()),
        )
        .unwrap();
        fs::write(
            dir.join("b.conf"),
            format!("[b]\nlocation = {}\n", b.display()),
        )
        .unwrap();
        // A non-conf file must be ignored.
        fs::write(dir.join("ignore.txt"), "junk").unwrap();

        let set = discover(&dir).unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.get("a").is_some());
        assert!(set.get("b").is_some());
    }

    #[test]
    fn missing_location_is_an_error() {
        let tmp = TempDir::new().unwrap();
        let conf = tmp.path().join("repos.conf");
        fs::write(&conf, "[broken]\npriority = 1\n").unwrap();
        let err = discover(&conf).unwrap_err();
        assert!(matches!(err, DiscoveryError::MissingLocation { repo } if repo == "broken"));
    }

    #[test]
    fn repos_conf_overrides_layout_masters() {
        let tmp = TempDir::new().unwrap();
        let base = make_repo(tmp.path(), "base", "");
        let other = make_repo(tmp.path(), "other", "");
        // layout.conf declares masters = other, repos.conf overrides with base.
        let child = make_repo(tmp.path(), "child", "masters = other\n");
        let conf = tmp.path().join("repos.conf");
        fs::write(
            &conf,
            format!(
                "[base]\nlocation = {}\n[other]\nlocation = {}\n[child]\nlocation = {}\nmasters = base\n",
                base.display(),
                other.display(),
                child.display()
            ),
        )
        .unwrap();
        let set = discover(&conf).unwrap();
        assert_eq!(set.get("child").unwrap().masters, vec!["base".to_owned()]);
    }

    #[test]
    fn layout_masters_used_when_repos_conf_silent() {
        let tmp = TempDir::new().unwrap();
        let base = make_repo(tmp.path(), "base", "");
        let child = make_repo(
            tmp.path(),
            "child",
            "masters = base\ncache-formats = md5-dict\n",
        );
        let conf = tmp.path().join("repos.conf");
        fs::write(
            &conf,
            format!(
                "[base]\nlocation = {}\n[child]\nlocation = {}\n",
                base.display(),
                child.display()
            ),
        )
        .unwrap();
        let set = discover(&conf).unwrap();
        assert_eq!(set.get("child").unwrap().masters, vec!["base".to_owned()]);
        assert_eq!(
            set.get("child").unwrap().cache_formats,
            vec!["md5-dict".to_owned()]
        );
    }

    #[test]
    fn master_searched_before_inheritor() {
        let tmp = TempDir::new().unwrap();
        let a = make_repo(tmp.path(), "a", "");
        let b = make_repo(tmp.path(), "b", "");
        let conf = tmp.path().join("repos.conf");
        fs::write(
            &conf,
            format!(
                "[a]\nlocation = {}\n[b]\nlocation = {}\nmasters = a\n",
                a.display(),
                b.display()
            ),
        )
        .unwrap();
        let set = discover(&conf).unwrap();
        let order = set.order();
        let ia = order.iter().position(|n| n == "a").unwrap();
        let ib = order.iter().position(|n| n == "b").unwrap();
        assert!(ia < ib, "master a must precede inheritor b: {order:?}");
    }

    #[test]
    fn priority_breaks_ties_deterministically() {
        let tmp = TempDir::new().unwrap();
        let lo = make_repo(tmp.path(), "lo", "");
        let hi = make_repo(tmp.path(), "hi", "");
        let conf = tmp.path().join("repos.conf");
        fs::write(
            &conf,
            format!(
                "[lo]\nlocation = {}\npriority = 1\n[hi]\nlocation = {}\npriority = 10\n",
                lo.display(),
                hi.display()
            ),
        )
        .unwrap();
        let set = discover(&conf).unwrap();
        // Higher priority searched first.
        assert_eq!(set.order(), &["hi".to_owned(), "lo".to_owned()]);
    }

    #[test]
    fn cyclic_masters_rejected() {
        let tmp = TempDir::new().unwrap();
        let a = make_repo(tmp.path(), "a", "");
        let b = make_repo(tmp.path(), "b", "");
        let conf = tmp.path().join("repos.conf");
        fs::write(
            &conf,
            format!(
                "[a]\nlocation = {}\nmasters = b\n[b]\nlocation = {}\nmasters = a\n",
                a.display(),
                b.display()
            ),
        )
        .unwrap();
        let err = discover(&conf).unwrap_err();
        match err {
            DiscoveryError::MastersCycle { repos } => {
                assert!(repos.contains('a') && repos.contains('b'));
            }
            other => panic!("expected cycle error, got {other:?}"),
        }
    }

    #[test]
    fn unknown_master_rejected() {
        let tmp = TempDir::new().unwrap();
        let a = make_repo(tmp.path(), "a", "");
        let conf = tmp.path().join("repos.conf");
        fs::write(
            &conf,
            format!("[a]\nlocation = {}\nmasters = ghost\n", a.display()),
        )
        .unwrap();
        let err = discover(&conf).unwrap_err();
        assert!(matches!(err, DiscoveryError::UnknownMaster { master, .. } if master == "ghost"));
    }

    #[test]
    fn eclass_search_path_prefers_override_then_self_then_master() {
        let tmp = TempDir::new().unwrap();
        let master = make_repo(tmp.path(), "master", "");
        let over = make_repo(tmp.path(), "over", "");
        let child = make_repo(tmp.path(), "child", "");
        let conf = tmp.path().join("repos.conf");
        fs::write(
            &conf,
            format!(
                "[master]\nlocation = {}\n[over]\nlocation = {}\n[child]\nlocation = {}\nmasters = master\neclass-overrides = over\n",
                master.display(),
                over.display(),
                child.display()
            ),
        )
        .unwrap();
        let set = discover(&conf).unwrap();
        let path = set.eclass_search_path("child");
        assert_eq!(
            path,
            vec![
                over.join("eclass"),
                child.join("eclass"),
                master.join("eclass"),
            ]
        );
    }

    #[test]
    fn repo_name_overrides_section_name() {
        let tmp = TempDir::new().unwrap();
        let loc = tmp.path().join("dir");
        fs::create_dir_all(loc.join("profiles")).unwrap();
        fs::write(loc.join("profiles/repo_name"), "canonical\n").unwrap();
        let conf = tmp.path().join("repos.conf");
        fs::write(&conf, format!("[section]\nlocation = {}\n", loc.display())).unwrap();
        let set = discover(&conf).unwrap();
        assert!(set.get("canonical").is_some());
    }

    #[test]
    fn malformed_ini_reported() {
        let tmp = TempDir::new().unwrap();
        let conf = tmp.path().join("repos.conf");
        fs::write(&conf, "[unterminated\nlocation = x\n").unwrap();
        assert!(matches!(discover(&conf), Err(DiscoveryError::Ini { .. })));
    }
}
