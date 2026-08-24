use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use aether_agent::{
    PlannerLimits, RepoMap, RepositoryActionPlan, RepositoryActionPlanner, RepositoryPlanRequest,
    RepositoryRequestKind,
};
use aether_core::{BoundedText, ContextSnapshot, DecisionEvidenceKind};
use criterion::{Criterion, criterion_group, criterion_main};

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

struct BenchRepo {
    path: PathBuf,
}

impl BenchRepo {
    fn new() -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("aether-planner-bench-{}-{id}", std::process::id()));
        fs::create_dir_all(path.join("src")).expect("planner bench directory");
        fs::write(
            path.join("src/lib.rs"),
            "pub fn parse_port(value: &str) -> Option<u16> { value.parse::<u16>().ok() }\n",
        )
        .expect("planner bench source");
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

fn context() -> ContextSnapshot {
    let mut context = ContextSnapshot::new("/workspace", None);
    context.current_task = BoundedText::new("fix parse_port", 4096);
    context.workflow.decision.upsert_candidate("src/lib.rs", 120, 2, false, false, false);
    context.workflow.decision.record_evidence(
        DecisionEvidenceKind::Symbol,
        "src/lib.rs",
        "parse_port at line 1",
        96,
        0,
    );
    context
}

fn plan() -> (RepositoryActionPlanner, ContextSnapshot, RepositoryActionPlan) {
    let context = context();
    let planner = RepositoryActionPlanner::new(PlannerLimits::default());
    let plan = planner.plan(RepositoryPlanRequest::new(
        RepositoryRequestKind::Search,
        "parse_port",
        &context,
    ));
    (planner, context, plan)
}

fn benchmark_planning(c: &mut Criterion) {
    let (planner, context, _) = plan();
    c.bench_function("repository_planner/pure_high_confidence_plan", |bencher| {
        bencher.iter(|| {
            std::hint::black_box(planner.plan(RepositoryPlanRequest::new(
                RepositoryRequestKind::Search,
                "parse_port",
                std::hint::black_box(&context),
            )))
        });
    });
}

fn benchmark_execution(c: &mut Criterion) {
    let repo = BenchRepo::new();
    let map = RepoMap::new(&repo.path);
    map.snapshot().expect("warm repo map");
    let (planner, context, plan) = plan();
    let cancellation = aether_agent::CancellationToken::new();
    c.bench_function("repository_planner/bounded_execution", |bencher| {
        bencher.iter(|| {
            let result = planner.execute(
                std::hint::black_box(&plan),
                &map,
                std::hint::black_box(&context),
                &cancellation,
            );
            std::hint::black_box(result)
        });
    });
}

criterion_group!(benches, benchmark_planning, benchmark_execution);
criterion_main!(benches);
