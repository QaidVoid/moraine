//! Structural benchmarks for merge-order serialization over a synthetic graph.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use moraine_resolve::serialize;
use moraine_resolve::solution::{DepClass, DepEdge, ResolvedPackage, ResolvedSolution, Root};
use moraine_version::Version;

fn linear_solution(n: usize) -> ResolvedSolution {
    let mut packages = Vec::with_capacity(n);
    let mut edges = Vec::new();
    for i in 0..n {
        packages.push(ResolvedPackage {
            cp: format!("cat/p{i:04}"),
            version: Version::parse("1").unwrap(),
            slot: "0".to_owned(),
            subslot: None,
            use_enabled: Default::default(),
            slot_bindings: Vec::new(),
            already_installed: false,
            subslot_rebuild: false,
        });
        if i > 0 {
            edges.push(DepEdge {
                from: format!("cat/p{i:04}"),
                to: format!("cat/p{:04}", i - 1),
                class: DepClass::Depend,
                root: Root::Target,
                build_time: true,
                slot_op: false,
                optional: false,
            });
        }
    }
    ResolvedSolution {
        packages,
        edges,
        blockers: Vec::new(),
    }
}

fn bench(c: &mut Criterion) {
    let sol = linear_solution(200);
    c.bench_function("serialize_linear_200", |b| {
        b.iter(|| {
            let _ = black_box(serialize(black_box(&sol)));
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
