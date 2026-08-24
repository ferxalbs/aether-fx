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

criterion_group!(benches, benchmark_parse_one_file, benchmark_lookup_one_thousand_symbols);
criterion_main!(benches);
