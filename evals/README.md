# Deterministic coding-agent evaluations

Run the offline deterministic suite with:

```text
cargo run --release -p aether-eval -- target/aether-eval-results.json
```

Each task declares its prompt, file outcomes, exact verification command, model-step budget,
tool-call budget, and verification-attempt budget in `crates/aether-eval/src/lib.rs`. The suite
also includes a shell-heavy command-reuse scenario with repeated direct `rg`, `find`, and `cat`
argv calls so command intelligence can be measured without shell parsing or network access.
Fixtures are
small standalone Rust workspaces. The runner copies one into a new temporary directory for every
task and removes the copy afterward, so runs never mutate fixture sources or reuse build state.

The default backend is a deterministic offline trace replay, but each trace is executed by the
production `Agent`, policy, scheduler, `ContextEngine`, nine-tool registry, and real fixture
filesystem. It performs no provider or network calls and sets Cargo offline for fixture
verification. `EvalBackend` remains the trace-generation boundary for real providers; provider-
backed evaluations should use a separate binary or feature so default CI remains offline and
reproducible.

JSON output includes per-task correctness, model requests, tokens when the provider reports them,
requested and executed calls, prevented calls, bytes read, bytes shown to the model, context bytes,
process spawns, verification attempts, wall/CPU time, and platform probes for RSS and allocations.
The `relationship-heavy-exploration` scenario requires repeated search-to-read navigation in
baseline mode and exercises the bounded planner in policy mode. Regression gates cover correctness,
steps, executed calls, and context size; shell-heavy and planner scenarios report before/after
metrics in the same result for direct comparison. Wall time is diagnostic only because shared CI
timing is unstable; RSS is measured by the external comparison wrapper, while unavailable in-process
probes are reported as zero.
