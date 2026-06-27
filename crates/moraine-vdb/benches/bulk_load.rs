//! Bulk-load benchmark for the installed store.
//!
//! Corpus-dependent: it imports the `/var/db/pkg` tree named by the
//! `MORAINE_CORPUS` environment variable into a temporary store, then measures a
//! full bulk load. When `MORAINE_CORPUS` is unset the benchmark registers
//! nothing and exits cleanly, so the gate stays green without a corpus.

use criterion::{Criterion, criterion_group, criterion_main};
use moraine_common::Interner;
use moraine_vdb::store::{Store, StorePaths};

fn bench_bulk_load(c: &mut Criterion) {
    let Some(root) = std::env::var_os("MORAINE_CORPUS").filter(|v| !v.is_empty()) else {
        return;
    };
    let vdb = std::path::PathBuf::from(root).join("var/db/pkg");
    if !vdb.is_dir() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let paths = StorePaths::in_dir(dir.path());

    let interner = std::sync::Arc::new(Interner::new());
    let records = moraine_vdb::import_vdb(&vdb, &interner).expect("import corpus");

    let mut store = Store::from_records(paths.clone(), interner, records);
    store.compact().expect("compact");

    c.bench_function("bulk_load", |b| {
        b.iter(|| {
            let store = Store::load(paths.clone()).expect("load");
            std::hint::black_box(store.records().len());
        });
    });
}

criterion_group!(benches, bench_bulk_load);
criterion_main!(benches);
