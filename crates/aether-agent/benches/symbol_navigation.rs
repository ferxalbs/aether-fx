use std::hint::black_box;

use aether_agent::{SymbolIndex, parse_rust};
use criterion::{Criterion, criterion_group, criterion_main};

fn benchmark_parse_one_file(c: &mut Criterion) {
    let source = r#"
        use crate::cache::Cache;
        pub mod navigation {
            pub struct Navigator;
            impl Navigator {
                pub fn lookup(&self) {}
            }
        }
        #[test]
        fn navigation_is_bounded() {}
    "#;
    c.bench_function("symbol_parse_one_rust_file", |bencher| {
        bencher.iter(|| black_box(parse_rust("src/navigation.rs", black_box(source))))
    });
}

fn benchmark_lookup_one_thousand_symbols(c: &mut Criterion) {
    let mut index = SymbolIndex::new();
    for file_index in 0..1_000 {
        let source = format!("pub fn navigation_target_{file_index}() {{}}\n");
        index.index_file(format!("src/generated_{file_index}.rs"), &source);
    }
    c.bench_function("symbol_lookup_1k_indexed_symbols", |bencher| {
        bencher.iter(|| black_box(index.lookup(black_box("navigation_target_777"), 8)))
    });
}

fn benchmark_relationship_lookup_ten_thousand_symbols(c: &mut Criterion) {
    let mut index = SymbolIndex::new();
    for file_index in 0..20 {
        let source = (0..500)
            .map(|symbol_index| {
                format!("pub fn navigation_target_{}_{}() {{}}\n", file_index, symbol_index)
            })
            .collect::<String>();
        index.index_file(format!("src/generated_{file_index}.rs"), &source);
    }
    c.bench_function("symbol_relationship_lookup_10k_symbols", |bencher| {
        bencher.iter(|| {
            black_box(index.lookup_relationships(black_box("navigation_target_17_499"), 8))
        })
    });
}

fn benchmark_relationship_update_ten_thousand_symbols(c: &mut Criterion) {
    let mut index = SymbolIndex::new();
    for file_index in 0..20 {
        let source = (0..500)
            .map(|symbol_index| {
                format!("pub fn navigation_target_{}_{}() {{}}\n", file_index, symbol_index)
            })
            .collect::<String>();
        index.index_file(format!("src/generated_{file_index}.rs"), &source);
    }
    let updated = "pub fn updated_navigation_target() {}\n";
    c.bench_function("symbol_relationship_update_one_file_10k_symbols", |bencher| {
        bencher.iter(|| {
            index.index_file("src/generated_17.rs", black_box(updated));
            black_box(index.lookup_relationships("updated_navigation_target", 8));
        })
    });
}

criterion_group!(
    benches,
    benchmark_parse_one_file,
    benchmark_lookup_one_thousand_symbols,
    benchmark_relationship_lookup_ten_thousand_symbols,
    benchmark_relationship_update_ten_thousand_symbols
);
criterion_main!(benches);
