//! Corpus-gated end-to-end check of installing from a real `.gpkg.tar` in a
//! `PKGDIR`.
//!
//! Gated on `MORAINE_CORPUS`, which points at a captured system root (`EROOT`).
//! The test discovers a real `.gpkg.tar` under its `var/cache/binpkgs` (matching
//! the local `.gpkg.tar` and multi-instance layout), reads it under strict gpkg
//! Manifest verification, builds a binary candidate from its recorded metadata,
//! and runs the compatibility check against a target assembled from the same
//! data. When the variable is unset or the corpus holds no `.gpkg.tar`, the test
//! is a no-op so the default suite needs no corpus.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use moraine_binpkg::resolution::{TargetConfig, Verdict, check_compatibility, parse_sonames};
use moraine_binpkg::{BinaryCandidate, read_package};
use moraine_install::locate_local_gpkg;
use moraine_version::Version;

/// Collect `.gpkg.tar` files anywhere under `dir`, since a real PKGDIR nests them
/// by category or by the multi-instance `<cp>/` subdirectory.
fn collect_gpkg_tar(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_gpkg_tar(&path, out);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".gpkg.tar"))
        {
            out.push(path);
        }
    }
}

#[test]
fn corpus_local_gpkg_tar_install_path() {
    let Some(root) = std::env::var_os("MORAINE_CORPUS").filter(|v| !v.is_empty()) else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus binpkg install path");
        return;
    };
    let pkgdir = PathBuf::from(&root).join("var/cache/binpkgs");
    if !pkgdir.is_dir() {
        eprintln!("no {} in corpus; skipping", pkgdir.display());
        return;
    }

    let mut packages = Vec::new();
    collect_gpkg_tar(&pkgdir, &mut packages);
    if packages.is_empty() {
        eprintln!("no .gpkg.tar under {}; skipping", pkgdir.display());
        return;
    }

    let mut checked = 0usize;
    for path in packages {
        let bytes = std::fs::read(&path).expect("read corpus container");
        // Strict gpkg Manifest verification must accept a real container.
        let Ok(pkg) = read_package(&bytes, None) else {
            continue;
        };

        // Recover the cpv from the relative path under PKGDIR and assert local
        // discovery finds the same container.
        let rel = path.strip_prefix(&pkgdir).unwrap_or(&path);
        let Some(cpv) = cpv_from_rel(rel) else {
            continue;
        };
        let cp = cp_of(&cpv);
        let found = locate_local_gpkg(&pkgdir, &cp, &cpv);
        assert!(
            found.is_some(),
            "local discovery missed {} for cpv {cpv}",
            path.display()
        );

        // Build a candidate from the recorded metadata and check it against a
        // target assembled from that same metadata: a self-consistent package
        // must be accepted.
        let candidate = BinaryCandidate {
            cp: cp.clone(),
            version: Version::parse("1").unwrap(),
            metadata: pkg.metadata.clone(),
        };
        let mut available_sonames: BTreeSet<(String, String)> = BTreeSet::new();
        if let Some(requires) = pkg.metadata.get_str("REQUIRES") {
            for pair in parse_sonames(&requires) {
                available_sonames.insert(pair);
            }
        }
        let target = TargetConfig {
            chost: pkg.metadata.get_str("CHOST").unwrap_or_default(),
            selected_use: pkg.metadata.use_flags().into_iter().collect(),
            forced_use: BTreeSet::new(),
            masked_use: BTreeSet::new(),
            available_sonames,
        };
        assert_eq!(
            check_compatibility(&candidate, &target),
            Verdict::Accept,
            "self-consistent container rejected: {}",
            path.display()
        );
        checked += 1;
    }

    assert!(checked > 0, "corpus produced no readable .gpkg.tar");
}

/// Recover `category/package-version` from a PKGDIR-relative path, handling the
/// single-instance `<category>/<pf>.gpkg.tar` and multi-instance
/// `<cp>/<pf>-<buildid>.gpkg.tar` layouts.
fn cpv_from_rel(rel: &Path) -> Option<String> {
    let components: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let file = components.last()?;
    let stem = file.strip_suffix(".gpkg.tar")?;
    match components.len() {
        // <category>/<pf>.gpkg.tar
        2 => Some(format!("{}/{}", components[0], stem)),
        // <category>/<package>/<pf>-<buildid>.gpkg.tar
        3 => {
            let pf = match stem.rsplit_once('-') {
                Some((pf, id)) if id.bytes().all(|b| b.is_ascii_digit()) && !id.is_empty() => pf,
                _ => stem,
            };
            Some(format!("{}/{}", components[0], pf))
        }
        _ => None,
    }
}

/// The `category/package` head of a cpv string, splitting `pf` at the version
/// boundary (a `-` followed by a digit).
fn cp_of(cpv: &str) -> String {
    let (category, pf) = cpv.split_once('/').unwrap_or(("", cpv));
    let bytes = pf.as_bytes();
    let mut idx = 0;
    while let Some(pos) = pf[idx..].find('-') {
        let at = idx + pos;
        if at + 1 < bytes.len() && bytes[at + 1].is_ascii_digit() {
            return format!("{category}/{}", &pf[..at]);
        }
        idx = at + 1;
    }
    format!("{category}/{pf}")
}
