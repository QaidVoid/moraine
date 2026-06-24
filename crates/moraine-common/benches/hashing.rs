//! Benchmarks for the content checksum hot path.
//!
//! Importing repository and installed metadata hashes large numbers of files,
//! so checksum throughput matters. This compares the greenfield BLAKE3 hash
//! against the Gentoo-compatibility hashes over a representative buffer.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use moraine_common::hash;

fn bench_hashes(c: &mut Criterion) {
    let data = vec![0xa5u8; 64 * 1024];
    let mut group = c.benchmark_group("hash_64k");
    group.bench_function("blake3", |b| b.iter(|| hash::blake3(black_box(&data))));
    group.bench_function("blake2b", |b| b.iter(|| hash::blake2b(black_box(&data))));
    group.bench_function("sha512", |b| b.iter(|| hash::sha512(black_box(&data))));
    group.finish();
}

criterion_group!(benches, bench_hashes);
criterion_main!(benches);
