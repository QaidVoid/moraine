//! Property tests for the metadata map round-trip and index name translations.

use moraine_binpkg::greenfield::{Reader, WriteOptions, write_bytes};
use moraine_binpkg::index::{PackageEntry, PackagesIndex};
use moraine_binpkg::metadata::{KEY_DESCRIPTION, KEY_MTIME, KEY_REPOSITORY, MetadataMap};
use moraine_common::Interner;
use proptest::prelude::*;

fn key_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[A-Z_][A-Z0-9_]{0,15}").unwrap()
}

fn value_strategy() -> impl Strategy<Value = String> {
    // Avoid leading or trailing whitespace: the index serialization trims values
    // at the `KEY: VALUE` boundary, so surrounding spaces are not preserved.
    proptest::string::string_regex("[a-zA-Z0-9./:_-]([a-zA-Z0-9 ./:_-]{0,38}[a-zA-Z0-9./:_-])?")
        .unwrap()
}

proptest! {
    #[test]
    fn greenfield_metadata_round_trips(
        entries in prop::collection::vec((key_strategy(), value_strategy()), 0..12),
        image in prop::collection::vec(any::<u8>(), 0..256),
    ) {
        let mut meta = MetadataMap::new();
        for (k, v) in &entries {
            meta.set_str(k, v);
        }
        let bytes = write_bytes(&meta, &image, &WriteOptions::default()).unwrap();
        let reader = Reader::open(&bytes).unwrap();
        prop_assert_eq!(reader.metadata().unwrap(), meta);
        prop_assert_eq!(reader.image().unwrap(), image);
    }

    #[test]
    fn index_name_translation_round_trips(
        desc in value_strategy(),
        repo in proptest::string::string_regex("[a-z][a-z0-9-]{0,15}").unwrap(),
        mtime in 0u64..2_000_000_000,
    ) {
        let interner = Interner::new();
        let mut meta = MetadataMap::new();
        meta.set_str(KEY_DESCRIPTION, &desc);
        meta.set_str(KEY_REPOSITORY, &repo);
        meta.set_str(KEY_MTIME, mtime.to_string());
        meta.set_str("SLOT", "0");

        let mut index = PackagesIndex::new();
        index.packages.push(PackageEntry {
            cpv: "cat/pkg-1".to_string(),
            metadata: meta,
        });

        let text = index.emit(&interner);
        // Index form uses translated names.
        prop_assert!(text.contains("DESC: "));
        prop_assert!(text.contains("REPO: "));
        prop_assert!(text.contains("MTIME: "));

        let parsed = PackagesIndex::parse(&text).unwrap();
        let back = &parsed.packages[0].metadata;
        // In-memory form uses canonical names.
        let got_desc = back.get_str(KEY_DESCRIPTION);
        let got_repo = back.get_str(KEY_REPOSITORY);
        prop_assert_eq!(got_desc.as_deref(), Some(desc.trim()));
        prop_assert_eq!(got_repo.as_deref(), Some(repo.as_str()));
        prop_assert_eq!(back.get_str(KEY_MTIME), Some(mtime.to_string()));
    }
}
