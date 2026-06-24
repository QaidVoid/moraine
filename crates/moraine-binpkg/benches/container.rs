//! Benchmarks for greenfield container write and metadata read.

use criterion::{Criterion, criterion_group, criterion_main};
use moraine_binpkg::greenfield::{Reader, WriteOptions, write_bytes};
use moraine_binpkg::metadata::{KEY_CHOST, KEY_USE, MetadataMap};
use std::hint::black_box;

fn sample() -> (MetadataMap, Vec<u8>) {
    let mut meta = MetadataMap::new();
    meta.set_str(KEY_CHOST, "x86_64-pc-linux-gnu");
    meta.set_str(KEY_USE, "ssl zlib threads");
    meta.set_str("SLOT", "0");
    meta.set_str("EAPI", "8");
    meta.set_str("RDEPEND", "dev-libs/openssl sys-libs/zlib");
    let image = b"PRETEND-IMAGE-TAR-CONTENT".repeat(4096);
    (meta, image)
}

fn bench_write(c: &mut Criterion) {
    let (meta, image) = sample();
    c.bench_function("greenfield_write", |b| {
        b.iter(|| {
            let bytes = write_bytes(
                black_box(&meta),
                black_box(&image),
                &WriteOptions::default(),
            )
            .unwrap();
            black_box(bytes);
        });
    });
}

fn bench_metadata_read(c: &mut Criterion) {
    let (meta, image) = sample();
    let bytes = write_bytes(&meta, &image, &WriteOptions::default()).unwrap();
    c.bench_function("greenfield_metadata_read", |b| {
        b.iter(|| {
            let reader = Reader::open(black_box(&bytes)).unwrap();
            black_box(reader.metadata().unwrap());
        });
    });
}

criterion_group!(benches, bench_write, bench_metadata_read);
criterion_main!(benches);
