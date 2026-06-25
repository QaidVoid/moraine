//! Assembling a [`ResolvedConfig`] from the profile stack and `/etc/portage`.
//!
//! This is the loader that the resolver needs: it folds global and per-package
//! USE, USE masking and forcing, package masking and unmasking, externally
//! provided packages, and accepted keywords into the managers a
//! [`ResolvedConfig`] holds. Profile nodes are read in stack order (parents
//! before children) and `/etc/portage` is applied last, matching Portage's
//! layering.
//!
//! Atoms are parsed against the caller-supplied interner so the resolved
//! configuration's symbols compare equal to a repository index built against the
//! same interner. Parsing every file with one shared interner is what makes
//! masking and USE actually apply during resolution.

use std::path::{Path, PathBuf};

use moraine_atom::Atom;
use moraine_common::Interner;
use moraine_eapi::{EapiFeatures, PERMISSIVE, features_for};

use crate::makeconf::VarMap;
use crate::profile::ProfileStack;
use crate::snapshot::ResolvedConfig;
use crate::use_resolution::{PkgUseEntry, UseManager, global_use, iuse_effective};
use crate::visibility::{MaskBuilder, ProvidedManager, parse_mask_pattern};

/// A repository's masking input: its name (for `::repo` scoping), its default
/// profile EAPI (for parsing its mask atoms), and the ordered `profiles`
/// directories whose `package.mask`/`package.unmask` to stack (masters first,
/// then the repository itself).
#[derive(Debug, Clone)]
pub struct RepoMaskInput {
    /// The repository name, used to scope its masks with `::repo`.
    pub name: String,
    /// The repository's default profile EAPI, if known.
    pub eapi: Option<String>,
    /// The `profiles` directories to stack masks from, masters first.
    pub profiles_dirs: Vec<PathBuf>,
}

/// The EAPI feature set for an optional EAPI string, permissive when absent.
fn features_for_opt(eapi: Option<&str>) -> EapiFeatures {
    eapi.map(features_for).unwrap_or(PERMISSIVE)
}

/// Assemble a [`ResolvedConfig`] from the active profile stack, the merged
/// environment, and `/etc/portage` under `config_root`.
///
/// `env` is the merged `make.defaults` plus `make.conf` variable map. `system`
/// and `world` are the resolved set members. Atoms are parsed against `interner`.
pub fn resolve_config(
    profile: &ProfileStack,
    env: &VarMap,
    config_root: &Path,
    repo_masks: &[RepoMaskInput],
    system: Vec<String>,
    world: Vec<String>,
    interner: &Interner,
) -> ResolvedConfig {
    let arch = env.get("ARCH").unwrap_or_default().to_owned();
    let global = global_use(env);

    // USE masking/forcing fold across the profile stack.
    let mut use_mask = StackSet::default();
    let mut use_force = StackSet::default();
    let mut use_stable_mask = StackSet::default();
    for node in &profile.nodes {
        use_mask.apply(&read_flag_file(&node.path.join("use.mask")));
        use_force.apply(&read_flag_file(&node.path.join("use.force")));
        // `use.stable.mask` is only honored from EAPI 5+ profile nodes.
        if node_level(node) >= 5 {
            use_stable_mask.apply(&read_flag_file(&node.path.join("use.stable.mask")));
        }
    }

    let mut use_manager = UseManager::new(global.enabled, global.hidden)
        .with_disabled(global.disabled)
        .with_mask(use_mask.into_sorted())
        .with_force(use_force.into_sorted())
        .with_stable_mask(use_stable_mask.into_sorted(), true)
        .with_iuse_effective(iuse_effective(env));

    // package.use across the profile stack then /etc/portage.
    for node in &profile.nodes {
        for line in read_lines(&node.path.join("package.use")) {
            if let Some(entry) = parse_pkg_use(&line, interner) {
                use_manager.add_pkg_use(entry);
            }
        }
    }
    for line in read_lines(&config_root.join("etc/portage/package.use")) {
        if let Some(entry) = parse_pkg_use(&line, interner) {
            use_manager.add_pkg_use(entry);
        }
    }

    // Per-package USE masking and forcing across the profile stack then
    // /etc/portage. Each file shares the `atom flag -flag` syntax of package.use.
    type AddFn = fn(&mut UseManager, PkgUseEntry);
    let pkg_use_files: [(&str, AddFn); 4] = [
        ("package.use.mask", UseManager::add_pkg_mask),
        ("package.use.force", UseManager::add_pkg_force),
        ("package.use.stable.mask", UseManager::add_pkg_stable_mask),
        ("package.use.stable.force", UseManager::add_pkg_stable_force),
    ];
    for (name, add) in pkg_use_files {
        for node in &profile.nodes {
            for line in read_lines(&node.path.join(name)) {
                if let Some(entry) = parse_pkg_use(&line, interner) {
                    add(&mut use_manager, entry);
                }
            }
        }
        for line in read_lines(&config_root.join("etc/portage").join(name)) {
            if let Some(entry) = parse_pkg_use(&line, interner) {
                add(&mut use_manager, entry);
            }
        }
    }

    // Package masking, lowest layer first:
    //   1. repository-wide masks (per repo, stacked over masters, `::repo`-scoped),
    //   2. the selected profile chain (global incremental stack),
    //   3. `/etc/portage` (plain lines mask, `-atoms` are standing unmasks).
    let mut mask_builder = MaskBuilder::new();

    for repo in repo_masks {
        let repo_sym = interner.intern(&repo.name);
        let features = features_for_opt(repo.eapi.as_deref());
        for token in stack_mask_tokens(&repo.profiles_dirs, "package.mask") {
            if let Some(pattern) = parse_mask_pattern(&token, interner, features) {
                mask_builder.push(&token, pattern, Some((&repo.name, repo_sym)));
            }
        }
        for dir in &repo.profiles_dirs {
            for line in read_lines(&dir.join("package.unmask")) {
                let text = line.strip_prefix('-').unwrap_or(&line);
                if let Some(pattern) = parse_mask_pattern(text, interner, features) {
                    mask_builder.add_standing_unmask(pattern);
                }
            }
        }
    }

    for node in &profile.nodes {
        let features = features_for(&node.eapi);
        for line in read_lines(&node.path.join("package.mask")) {
            apply_profile_mask_line(&mut mask_builder, &line, interner, features);
        }
        for line in read_lines(&node.path.join("package.unmask")) {
            let text = line.strip_prefix('-').unwrap_or(&line);
            if let Some(pattern) = parse_mask_pattern(text, interner, features) {
                mask_builder.add_standing_unmask(pattern);
            }
        }
    }

    for line in read_lines(&config_root.join("etc/portage/package.mask")) {
        if line == "-*" {
            mask_builder.clear();
        } else if let Some(rest) = line.strip_prefix('-') {
            if let Some(pattern) = parse_mask_pattern(rest, interner, PERMISSIVE) {
                mask_builder.add_standing_unmask(pattern);
            }
        } else if let Some(pattern) = parse_mask_pattern(&line, interner, PERMISSIVE) {
            mask_builder.push(&line, pattern, None);
        }
    }
    for line in read_lines(&config_root.join("etc/portage/package.unmask")) {
        let text = line.strip_prefix('-').unwrap_or(&line);
        if let Some(pattern) = parse_mask_pattern(text, interner, PERMISSIVE) {
            mask_builder.add_standing_unmask(pattern);
        }
    }

    let mask_manager = mask_builder.build();

    // Externally provided packages.
    let mut provided = ProvidedManager::new();
    for node in &profile.nodes {
        // `package.provided` is banned in EAPI 7+ profile nodes.
        if node_level(node) >= 7 {
            continue;
        }
        for line in read_lines(&node.path.join("package.provided")) {
            // `package.provided` lines are bare `category/package-version`
            // CPVs, which are implicitly exact-version atoms.
            let text = if line.starts_with(['=', '<', '>', '~']) {
                line.clone()
            } else {
                format!("={line}")
            };
            if let Some(atom) = parse_atom(&text, interner) {
                provided.add(atom);
            }
        }
    }

    // Accepted keywords, defaulting to the profile arch.
    let mut accepted: std::collections::BTreeSet<String> = env
        .get("ACCEPT_KEYWORDS")
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if accepted.is_empty() && !arch.is_empty() {
        accepted.insert(arch.clone());
    }

    ResolvedConfig::new(
        profile.clone(),
        arch,
        accepted,
        use_manager,
        mask_manager,
        provided,
        system,
        world,
    )
}

/// The numeric EAPI level of a profile node, defaulting to 0 when unparseable.
fn node_level(node: &crate::profile::ProfileNode) -> u8 {
    moraine_eapi::level(&node.eapi).unwrap_or(0)
}

/// A set folded across stacked files, where a leading `-` removes a prior entry.
#[derive(Debug, Default)]
struct StackSet {
    set: std::collections::BTreeSet<String>,
}

impl StackSet {
    fn apply(&mut self, tokens: &[String]) {
        for token in tokens {
            if let Some(rest) = token.strip_prefix('-') {
                self.set.remove(rest);
            } else {
                self.set.insert(token.clone());
            }
        }
    }

    fn into_sorted(self) -> Vec<String> {
        self.set.into_iter().collect()
    }
}

/// Read whitespace-separated flag tokens from a flag file (one or more per
/// line), skipping comments and blank lines.
fn read_flag_file(path: &Path) -> Vec<String> {
    read_lines(path)
        .iter()
        .flat_map(|line| {
            line.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Read non-comment, non-blank lines from a path that may be a file or a
/// directory of files (read in sorted name order).
fn read_lines(path: &Path) -> Vec<String> {
    let mut bodies = Vec::new();
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            let mut files: Vec<_> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file())
                .collect();
            files.sort();
            for file in files {
                if let Ok(body) = std::fs::read_to_string(&file) {
                    bodies.push(body);
                }
            }
        }
    } else if let Ok(body) = std::fs::read_to_string(path) {
        bodies.push(body);
    }

    bodies
        .iter()
        .flat_map(|body| body.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// Parse a `package.use` line: `atom flag -flag ...`.
fn parse_pkg_use(line: &str, interner: &Interner) -> Option<PkgUseEntry> {
    let mut parts = line.split_whitespace();
    let atom_text = parts.next()?;
    let atom = parse_atom(atom_text, interner)?;
    let mods: Vec<(String, bool)> = parts
        .map(|flag| match flag.strip_prefix('-') {
            Some(rest) => (rest.to_owned(), false),
            None => (flag.to_owned(), true),
        })
        .collect();
    Some(PkgUseEntry { atom, mods })
}

/// Apply one profile-chain `package.mask` line to the builder: `-*` clears, a
/// `-atom` pops the matching prior mask, and a plain line pushes a mask.
fn apply_profile_mask_line(
    builder: &mut MaskBuilder,
    line: &str,
    interner: &Interner,
    features: EapiFeatures,
) {
    if line == "-*" {
        builder.clear();
    } else if let Some(rest) = line.strip_prefix('-') {
        builder.pop(rest);
    } else if let Some(pattern) = parse_mask_pattern(line, interner, features) {
        builder.push(line, pattern, None);
    }
}

/// Incrementally stack the tokens of `filename` across `dirs` (masters first):
/// a plain token is appended once, a `-token` removes the matching prior token,
/// and `-*` clears the accumulator, mirroring `stack_lists`.
fn stack_mask_tokens(dirs: &[PathBuf], filename: &str) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    for dir in dirs {
        for token in read_lines(&dir.join(filename)) {
            if token == "-*" {
                order.clear();
            } else if let Some(rest) = token.strip_prefix('-') {
                order.retain(|t| t != rest);
            } else if !order.iter().any(|t| t == &token) {
                order.push(token);
            }
        }
    }
    order
}

/// Parse one atom against `interner`, returning `None` on a parse error so a bad
/// line is skipped rather than failing the whole load.
fn parse_atom(text: &str, interner: &Interner) -> Option<Atom> {
    Atom::parse(text, PERMISSIVE, interner).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use moraine_atom::PackageRef;
    use moraine_version::Version;

    fn profile_with(dir: &Path) -> ProfileStack {
        profile_with_eapi(dir, "8")
    }

    fn profile_with_eapi(dir: &Path, eapi: &str) -> ProfileStack {
        ProfileStack {
            nodes: vec![crate::profile::ProfileNode {
                path: dir.to_path_buf(),
                eapi: eapi.to_owned(),
                is_user: false,
            }],
        }
    }

    fn pref<'a>(interner: &Interner, cat: &str, pkg: &str, version: &'a Version) -> PackageRef<'a> {
        PackageRef {
            category: interner.intern(cat),
            package: interner.intern(pkg),
            version,
            slot: None,
            subslot: None,
            repo: None,
        }
    }

    #[test]
    fn mask_and_unmask_layer() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.mask"), "dev-libs/broken\n").unwrap();
        let interner = Interner::new();
        let cfg = resolve_config(
            &profile_with(dir.path()),
            &VarMap::new(),
            dir.path(),
            &[],
            vec![],
            vec![],
            &interner,
        );
        let version = Version::parse("1.0").unwrap();
        assert!(cfg.is_masked(&pref(&interner, "dev-libs", "broken", &version)));
        assert!(!cfg.is_masked(&pref(&interner, "dev-libs", "fine", &version)));
    }

    #[test]
    fn etc_portage_unmask_wins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.mask"), "dev-libs/x\n").unwrap();
        std::fs::create_dir_all(dir.path().join("etc/portage")).unwrap();
        std::fs::write(
            dir.path().join("etc/portage/package.unmask"),
            "dev-libs/x\n",
        )
        .unwrap();
        let interner = Interner::new();
        let cfg = resolve_config(
            &profile_with(dir.path()),
            &VarMap::new(),
            dir.path(),
            &[],
            vec![],
            vec![],
            &interner,
        );
        let version = Version::parse("1.0").unwrap();
        assert!(!cfg.is_masked(&pref(&interner, "dev-libs", "x", &version)));
    }

    #[test]
    fn repo_wide_mask_applies_and_is_repo_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let profiles = dir.path().join("repo/profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::write(profiles.join("package.mask"), "dev-libs/foo\n").unwrap();
        let interner = Interner::new();
        let repo_masks = vec![RepoMaskInput {
            name: "gentoo".to_owned(),
            eapi: Some("8".to_owned()),
            profiles_dirs: vec![profiles],
        }];
        let cfg = resolve_config(
            &ProfileStack::default(),
            &VarMap::new(),
            dir.path(),
            &repo_masks,
            vec![],
            vec![],
            &interner,
        );
        let version = Version::parse("1.0").unwrap();
        let gentoo = interner.intern("gentoo");
        let from_gentoo = PackageRef {
            category: interner.intern("dev-libs"),
            package: interner.intern("foo"),
            version: &version,
            slot: None,
            subslot: None,
            repo: Some(gentoo),
        };
        assert!(cfg.is_masked(&from_gentoo));
        // The same cp from another repository is not masked by gentoo's scope.
        let from_overlay = PackageRef {
            repo: Some(interner.intern("overlay")),
            ..from_gentoo
        };
        assert!(!cfg.is_masked(&from_overlay));
    }

    #[test]
    fn package_use_enables_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.use"), "dev-libs/openssl ssl\n").unwrap();
        let interner = Interner::new();
        let cfg = resolve_config(
            &profile_with(dir.path()),
            &VarMap::new(),
            dir.path(),
            &[],
            vec![],
            vec![],
            &interner,
        );
        let version = Version::parse("3.0").unwrap();
        let eff = cfg.effective_use(
            &pref(&interner, "dev-libs", "openssl", &version),
            &[],
            false,
        );
        assert!(eff.enabled.contains("ssl"));
    }

    #[test]
    fn provided_package_is_recognized() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.provided"),
            "sys-kernel/gentoo-sources-6.6\n",
        )
        .unwrap();
        let interner = Interner::new();
        // `package.provided` is honored in pre-7 EAPI profiles.
        let cfg = resolve_config(
            &profile_with_eapi(dir.path(), "6"),
            &VarMap::new(),
            dir.path(),
            &[],
            vec![],
            vec![],
            &interner,
        );
        let version = Version::parse("6.6").unwrap();
        assert!(cfg.is_provided(&pref(&interner, "sys-kernel", "gentoo-sources", &version)));
    }

    #[test]
    fn package_provided_rejected_in_eapi_7() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.provided"),
            "sys-kernel/gentoo-sources-6.6\n",
        )
        .unwrap();
        let interner = Interner::new();
        let cfg = resolve_config(
            &profile_with_eapi(dir.path(), "7"),
            &VarMap::new(),
            dir.path(),
            &[],
            vec![],
            vec![],
            &interner,
        );
        let version = Version::parse("6.6").unwrap();
        assert!(!cfg.is_provided(&pref(&interner, "sys-kernel", "gentoo-sources", &version)));
    }

    #[test]
    fn use_stable_mask_gated_by_eapi() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("use.stable.mask"), "exp\n").unwrap();
        let interner = Interner::new();
        let version = Version::parse("1.0").unwrap();

        // EAPI 4 node: use.stable.mask is ignored, so a stable build keeps `exp`.
        let cfg4 = resolve_config(
            &profile_with_eapi(dir.path(), "4"),
            &VarMap::new(),
            dir.path(),
            &[],
            vec![],
            vec![],
            &interner,
        );
        let eff = cfg4.effective_use(
            &pref(&interner, "a", "b", &version),
            &["+exp".to_owned()],
            true,
        );
        assert!(eff.enabled.contains("exp"));

        // EAPI 8 node: use.stable.mask applies, masking `exp` for stable builds.
        let cfg8 = resolve_config(
            &profile_with_eapi(dir.path(), "8"),
            &VarMap::new(),
            dir.path(),
            &[],
            vec![],
            vec![],
            &interner,
        );
        let eff = cfg8.effective_use(
            &pref(&interner, "a", "b", &version),
            &["+exp".to_owned()],
            true,
        );
        assert!(!eff.enabled.contains("exp"));
    }

    #[test]
    fn accept_keywords_defaults_to_arch() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = VarMap::new();
        env.set("ARCH".to_owned(), "amd64".to_owned());
        let interner = Interner::new();
        let cfg = resolve_config(
            &profile_with(dir.path()),
            &env,
            dir.path(),
            &[],
            vec![],
            vec![],
            &interner,
        );
        let accepted = cfg.keyword_result(&["amd64".to_owned()], &[]);
        assert!(matches!(
            accepted,
            crate::visibility::KeywordResult::Accepted
        ));
    }
}
