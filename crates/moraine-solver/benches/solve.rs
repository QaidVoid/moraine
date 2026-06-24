//! Benchmarks: a conflict-heavy universe and a large linear universe.
//!
//! The conflict-heavy case confirms that learned incompatibilities keep the
//! search from rediscovering the same conflicts; the large case validates lazy,
//! best-first candidate exploration over many packages.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use moraine_solver::{MapProvider, Range, Term, solve};

fn conflict_heavy() -> MapProvider<u32> {
    // root depends on every p_i; each p_i has versions that constrain a shared
    // package `shared` to a narrow band, forcing backjumping and learning.
    let mut p = MapProvider::new();
    let n = 12u32;
    p.add_package(0, vec![1]); // root = 0
    let shared = 1000u32;
    p.add_package(shared, (1..=40).collect());
    let mut root_deps = Vec::new();
    for i in 1..=n {
        p.add_package(i, vec![1, 2, 3]);
        root_deps.push((i, Term::positive(Range::full())));
        // Each high version demands shared in a band that conflicts with others.
        for v in 1..=3u32 {
            let lo = i * 2;
            p.add_dependency(
                i,
                v,
                vec![(
                    shared,
                    Term::positive(Range::interval(
                        std::ops::Bound::Included(lo),
                        std::ops::Bound::Excluded(lo + 5),
                    )),
                )],
            );
        }
    }
    p.add_dependency(0, 1, root_deps);
    p
}

fn large_linear() -> MapProvider<u32> {
    // root -> p1 -> p2 -> ... -> pN, each with several versions.
    let mut p = MapProvider::new();
    let n = 300u32;
    p.add_package(0, vec![1]);
    p.add_dependency(0, 1, vec![(1, Term::positive(Range::full()))]);
    for i in 1..=n {
        p.add_package(i, vec![1, 2, 3, 4, 5]);
        if i < n {
            for v in 1..=5u32 {
                p.add_dependency(i, v, vec![(i + 1, Term::positive(Range::full()))]);
            }
        }
    }
    p
}

fn bench(c: &mut Criterion) {
    let ch = conflict_heavy();
    c.bench_function("solve_conflict_heavy", |b| {
        b.iter(|| {
            let _ = black_box(solve(&ch, 0, 1));
        });
    });

    let large = large_linear();
    c.bench_function("solve_large_linear", |b| {
        b.iter(|| {
            let _ = solve(black_box(&large), 0, 1).unwrap();
        });
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
