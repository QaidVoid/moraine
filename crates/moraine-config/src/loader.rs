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

use std::path::Path;

use moraine_atom::Atom;
use moraine_common::Interner;
use moraine_eapi::PERMISSIVE;

use crate::makeconf::VarMap;
use crate::profile::ProfileStack;
use crate::snapshot::ResolvedConfig;
use crate::use_resolution::{PkgUseEntry, UseManager, global_use};
use crate::visibility::{MaskManager, ProvidedManager};

/// Assemble a [`ResolvedConfig`] from the active profile stack, the merged
/// environment, and `/etc/portage` under `config_root`.
///
/// `env` is the merged `make.defaults` plus `make.conf` variable map. `system`
/// and `world` are the resolved set members. Atoms are parsed against `interner`.
pub fn resolve_config(
    profile: &ProfileStack,
    env: &VarMap,
    config_root: &Path,
    system: Vec<String>,
    world: Vec<String>,
    interner: &Interner,
) -> ResolvedConfig {
    let arch = env.get("ARCH").unwrap_or_default().to_owned();
    let (global, hidden) = global_use(env);

    // USE masking/forcing fold across the profile stack.
    let mut use_mask = StackSet::default();
    let mut use_force = StackSet::default();
    let mut use_stable_mask = StackSet::default();
    for node in &profile.nodes {
        use_mask.apply(&read_flag_file(&node.path.join("use.mask")));
        use_force.apply(&read_flag_file(&node.path.join("use.force")));
        use_stable_mask.apply(&read_flag_file(&node.path.join("use.stable.mask")));
    }

    let mut use_manager = UseManager::new(global, hidden)
        .with_mask(use_mask.into_sorted())
        .with_force(use_force.into_sorted())
        .with_stable_mask(use_stable_mask.into_sorted(), true);

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

    // Package masking: profile package.mask/unmask then /etc/portage.
    let mut mask_manager = MaskManager::new();
    for node in &profile.nodes {
        apply_mask_file(&mut mask_manager, &node.path.join("package.mask"), interner);
        apply_unmask_file(
            &mut mask_manager,
            &node.path.join("package.unmask"),
            interner,
        );
    }
    apply_mask_file(
        &mut mask_manager,
        &config_root.join("etc/portage/package.mask"),
        interner,
    );
    apply_unmask_file(
        &mut mask_manager,
        &config_root.join("etc/portage/package.unmask"),
        interner,
    );

    // Externally provided packages.
    let mut provided = ProvidedManager::new();
    for node in &profile.nodes {
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

/// Apply a `package.mask` file: plain lines mask, `-` lines unmask.
fn apply_mask_file(manager: &mut MaskManager, path: &Path, interner: &Interner) {
    for line in read_lines(path) {
        if let Some(rest) = line.strip_prefix('-') {
            if let Some(atom) = parse_atom(rest, interner) {
                manager.add_unmask(atom);
            }
        } else if let Some(atom) = parse_atom(&line, interner) {
            manager.add_mask(atom);
        }
    }
}

/// Apply a `package.unmask` file: every line unmasks.
fn apply_unmask_file(manager: &mut MaskManager, path: &Path, interner: &Interner) {
    for line in read_lines(path) {
        let text = line.strip_prefix('-').unwrap_or(&line);
        if let Some(atom) = parse_atom(text, interner) {
            manager.add_unmask(atom);
        }
    }
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
        ProfileStack {
            nodes: vec![crate::profile::ProfileNode {
                path: dir.to_path_buf(),
                eapi: "8".to_owned(),
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
            vec![],
            vec![],
            &interner,
        );
        let version = Version::parse("1.0").unwrap();
        assert!(!cfg.is_masked(&pref(&interner, "dev-libs", "x", &version)));
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
        let cfg = resolve_config(
            &profile_with(dir.path()),
            &VarMap::new(),
            dir.path(),
            vec![],
            vec![],
            &interner,
        );
        let version = Version::parse("6.6").unwrap();
        assert!(cfg.is_provided(&pref(&interner, "sys-kernel", "gentoo-sources", &version)));
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
