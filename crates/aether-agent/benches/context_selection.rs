use std::hint::black_box;

use aether_agent::{ContextCandidate, ContextKind, select_context};
use criterion::{Criterion, criterion_group, criterion_main};

fn benchmark_size(c: &mut Criterion, count: usize) {
    let storage: Vec<String> = (0..count)
        .map(|index| format!("crates/module_{index}/src/context_selector_{index}.rs"))
        .collect();
    let candidates: Vec<_> = storage
        .iter()
        .enumerate()
        .map(|(index, path)| ContextCandidate {
            kind: if index % 4 == 0 { ContextKind::Excerpt } else { ContextKind::InspectedFile },
            path: Some(path),
            content: if index % 17 == 0 { "select context budget" } else { "stored item" },
            start_line: index,
            end_line: index + 8,
            recency: index,
            modified: index % 97 == 0,
            stale: index % 211 == 0,
        })
        .collect();
    c.bench_function(&format!("context_selection_{count}_candidates"), |bencher| {
        bencher.iter(|| {
            black_box(select_context(black_box("context"), black_box(&candidates), 24 * 1024, 48))
        });
    });
}

fn context_selection(c: &mut Criterion) {
    benchmark_size(c, 100);
    benchmark_size(c, 1_000);
    benchmark_size(c, 10_000);
}

criterion_group!(benches, context_selection);
criterion_main!(benches);
