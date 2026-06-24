//! Corpus round-trip test importing real stock containers and re-emitting the
//! greenfield format, checking metadata field preservation.
//!
//! Gated on the `MORAINE_CORPUS` environment variable, which must name a
//! directory of real `.tbz2`, `.xpak`, and `.gpkg` binary packages. When the
//! variable is unset the test is a no-op so the default suite needs no corpus.

use std::path::PathBuf;

use moraine_binpkg::greenfield::{Reader, WriteOptions, write_bytes};
use moraine_binpkg::read_package;

#[test]
fn corpus_round_trip_preserves_metadata() {
    let Ok(root) = std::env::var("MORAINE_CORPUS") else {
        eprintln!("MORAINE_CORPUS unset; skipping corpus round-trip");
        return;
    };
    let root = PathBuf::from(root);
    let mut checked = 0usize;

    let entries = std::fs::read_dir(&root).expect("read corpus dir");
    for entry in entries {
        let path = entry.expect("corpus entry").path();
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "tbz2" | "xpak" | "gpkg" | "bz2" | "tar") {
            continue;
        }
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
