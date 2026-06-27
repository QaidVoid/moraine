//! Cold store load and atom candidate-selection benchmarks.
//!
//! The corpus-dependent benchmarks are gated on the `MORAINE_CORPUS` environment
//! variable, which must point at a directory containing a `repos.conf` (or a
//! `repos.conf` directory) and the referenced repository trees. Without it, the
//! benchmark group is a no-op so the harness still runs in CI.

use std::hint::black_box;
use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use moraine_repo::{build_index, store::read_entries};

fn corpus_dir() -> Option<PathBuf> {
    std::env::var_os("MORAINE_CORPUS").map(PathBuf::from)
}

fn bench_store(c: &mut Criterion) {
    let Some(corpus) = corpus_dir() else {
        // No corpus configured: nothing to benchmark.
        return;
    };
    let repos_conf = corpus.join("etc/portage/repos.conf");
    if !repos_conf.exists() {
        return;
    }
    let store = tempfile::tempdir().expect("temp store dir");
    let store_dir = store.path().to_path_buf();

    // Cold import-and-load: builds every store from md5-cache and loads them.
    c.bench_function("build_index_cold", |b| {
        b.iter(|| {
            let index =
                build_index(black_box(&repos_conf), black_box(&store_dir)).expect("build index");
            black_box(index.repos().len());
        });
    });

    // Build once, then benchmark a warm load of a single store.
    let index = build_index(&repos_conf, &store_dir).expect("build index");
    if let Some(first) = index.repos().first() {
        let store_path = store_dir.join(format!("{}.mrepo", first.name));
        c.bench_function("read_entries_warm", |b| {
            b.iter(|| {
                let entries = read_entries(black_box(&store_path)).expect("read entries");
                black_box(entries.len());
            });
        });

        // Candidate selection on the loaded store.
        c.bench_function("match_atom", |b| {
            b.iter(|| {
                let cands = index.match_atom_str(black_box("sys-apps/portage"));
                black_box(cands.len());
            });
        });
    }
}

criterion_group!(benches, bench_store);
criterion_main!(benches);
