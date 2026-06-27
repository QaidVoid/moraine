//! Corpus round-trip test importing real stock containers and re-emitting the
//! greenfield format, checking metadata field preservation.
//!
//! Gated on the `MORAINE_CORPUS` environment variable, which points at a
//! captured system root (`EROOT`). The binary packages are read from its
//! `var/cache/binpkgs` (PKGDIR). When the variable is unset, the PKGDIR is
//! absent, or it holds no importable packages, the test is a no-op so the
//! default suite needs no corpus.

use std::path::{Path, PathBuf};

use moraine_binpkg::greenfield::{Reader, WriteOptions, write_bytes};
use moraine_binpkg::read_package;

/// Collect candidate binary-package files anywhere under `dir`, since a real
/// PKGDIR nests them by category.
fn collect_packages(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_packages(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()).unwrap_or(""),
            "tbz2" | "xpak" | "gpkg" | "bz2" | "tar"
        ) {
            out.push(path);
        }
    }
}

#[test]
fn corpus_round_trip_preserves_metadata() {
    let Some(root) = std::env::var_os("MORAINE_CORPUS").filter(|v| !v.is_empty()) else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus round-trip");
        return;
    };
    let pkgdir = PathBuf::from(root).join("var/cache/binpkgs");
    if !pkgdir.is_dir() {
        eprintln!(
            "no {} in corpus; skipping binpkg round-trip",
            pkgdir.display()
        );
        return;
    }

    let mut packages = Vec::new();
    collect_packages(&pkgdir, &mut packages);
    if packages.is_empty() {
        eprintln!("no binary packages under {}; skipping", pkgdir.display());
        return;
    }

    let mut checked = 0usize;
    for path in packages {
        let bytes = std::fs::read(&path).expect("read corpus file");
        let Ok(pkg) = read_package(&bytes, None) else {
            continue;
        };

        // Re-emit the imported metadata and image as greenfield, then read it
        // back and assert every metadata field survives the round-trip.
        let green = write_bytes(&pkg.metadata, &pkg.image, &WriteOptions::default())
            .expect("write greenfield");
        let reader = Reader::open(&green).expect("open greenfield");
        let back = reader.metadata().expect("read greenfield metadata");
        assert_eq!(
            back,
            pkg.metadata,
            "metadata not preserved for {}",
            path.display()
        );
        assert_eq!(
            reader.image().expect("read greenfield image"),
            pkg.image,
            "image not preserved for {}",
            path.display()
        );
        checked += 1;
    }

    assert!(checked > 0, "corpus produced no importable packages");
}
