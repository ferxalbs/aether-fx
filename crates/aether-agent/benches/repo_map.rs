use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use aether_agent::{RepoMap, RepoMapLimits};
use criterion::{Criterion, criterion_group, criterion_main};

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

struct BenchRepo {
    path: PathBuf,
}

impl BenchRepo {
    fn new(file_count: usize) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
        let path = std::env::temp_dir().join(format!("aether-repo-map-bench-{nanos}-{sequence}"));
        fs::create_dir_all(path.join("src")).expect("bench repo");
        for index in 0..file_count {
            fs::write(path.join(format!("src/file-{index}.rs")), "pub fn generated() {}\n")
                .expect("bench file");
        }
        git(&path, ["init", "--quiet"]);
        git(&path, ["add", "."]);
        Self { path }
    }
}

impl Drop for BenchRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn git<const N: usize>(root: &PathBuf, args: [&str; N]) {
    assert!(Command::new("git").current_dir(root).args(args).status().expect("git").success());
}

fn benchmark_cold_map(c: &mut Criterion, file_count: usize) {
    let repo = BenchRepo::new(file_count);
    let limits = RepoMapLimits { max_files: 2_048, ..RepoMapLimits::default() };
    c.bench_function(&format!("repo_map_cold_{file_count}_tracked_files"), |bencher| {
        bencher.iter(|| {
            let map = RepoMap::with_limits(&repo.path, limits);
            std::hint::black_box(map.snapshot().expect("repo map"));
        });
    });
}

fn benchmark_cached_map(c: &mut Criterion, file_count: usize) {
    let repo = BenchRepo::new(file_count);
    let map = RepoMap::new(&repo.path);
    map.snapshot().expect("warm map");
    c.bench_function(&format!("repo_map_cached_{file_count}_tracked_files"), |bencher| {
        bencher.iter(|| std::hint::black_box(map.snapshot().expect("repo map")));
    });
}

fn repo_map_1k(c: &mut Criterion) {
    benchmark_cold_map(c, 1_000);
    benchmark_cached_map(c, 1_000);
}

fn repo_map_10k(c: &mut Criterion) {
    benchmark_cold_map(c, 10_000);
    benchmark_cached_map(c, 10_000);
}

criterion_group!(benches, repo_map_1k, repo_map_10k);
criterion_main!(benches);
