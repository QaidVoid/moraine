//! Benchmark for dependency-string parse throughput.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use moraine_atom::DepSpec;
use moraine_common::Interner;
use moraine_eapi::features_for_level;

const DEP: &str = "|| ( dev-libs/openssl:0= >=dev-libs/libressl-3.0 ) \
    ssl? ( net-libs/gnutls[-static] ) \
    >=sys-libs/zlib-1.2.11:0 dev-libs/libxml2 \
    python? ( dev-lang/python:3.11 || ( dev-python/a dev-python/b ) )";

fn bench(c: &mut Criterion) {
    let features = features_for_level(8);
    c.bench_function("depstring_parse", |b| {
        b.iter(|| {
            let interner = Interner::new();
            let _ = DepSpec::parse(black_box(DEP), features, &interner).unwrap();
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
