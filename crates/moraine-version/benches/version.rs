//! Benchmarks for version parse and compare throughput.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use moraine_version::Version;

const SAMPLES: &[&str] = &[
    "1.2.3",
    "4.5.6_alpha1-r2",
    "10.0.1_p3",
    "2.38",
    "1.0.0_rc1",
    "0.9.8z_p1",
    "3.14.159-r7",
];

fn bench(c: &mut Criterion) {
    c.bench_function("version_parse", |b| {
        b.iter(|| {
            for s in SAMPLES {
                let _ = Version::parse(black_box(s)).unwrap();
            }
        });
    });

    let parsed: Vec<Version> = SAMPLES.iter().map(|s| Version::parse(s).unwrap()).collect();
    c.bench_function("version_cmp", |b| {
        b.iter(|| {
            let mut last = std::cmp::Ordering::Equal;
            for i in 0..parsed.len() {
                for j in 0..parsed.len() {
                    last = parsed[black_box(i)].cmp(&parsed[black_box(j)]);
                }
            }
            black_box(last)
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
