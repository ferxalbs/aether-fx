# Deterministic coding-agent evaluations

Run the offline deterministic suite with:

```text
cargo run --release -p aether-eval -- target/aether-eval-results.json
```

Run the separate hard-task capability suite with:

```text
cargo run --release -p aether-eval -- --capability target/aether-eval-capability-results.json
```

Default `cargo test` does not replay these suites. CI runs them once through this binary so
native workspace tests do not spawn nested `cargo test` for every fixture on every target.

The regression suite is intentionally unchanged and remains the 9/9 non-regression gate. The
capability suite measures autonomous progress across a serialized interruption boundary. Its five
offline scenarios cover multi-file/new-module implementation, compiler-error-driven repair,
cross-crate workspace migration, stale-observation recovery after an external change, and
unrelated user-note preservation in a dirty worktree. Each first segment is deliberately capped
before completion; the second segment receives only serialized, persistable context and the
remaining deterministic model trace. The final check reads the fixture and verifies the real
repository state, not just the event transcript.

Capability output is separate from efficiency and regression output. It reports success@1,
intentionally interrupted baseline success, newly solved tasks, checkpoint and WorkState survival,
multi-file correctness, typed recovery, stale-evidence rejections, user-change corruption,
verification attempts/failures, model requests, executed tools, process spawns, context and
model-visible bytes, wall time, and unresolved work at finish. The offline suite uses one fixed
deterministic trial per task; provider-backed repeated trials remain a separate integration concern
behind `EvalBackend` and are not fabricated by the offline runner.

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
requested and executed calls, prevented/reused observations, bytes read, bytes shown to the model,
context bytes, static-prefix/tool-schema/context/tool-result/policy-feedback/protocol byte
categories, process spawns, verification attempts, wall/provider/local time, and platform probes
for RSS and allocations. Each task also contains a bounded trajectory report that classifies
repeated observations, search-to-read chains, verification recovery, and mutation-before-
inspection candidates. The aggregate report merges those pattern counts for the baseline traces.
The `relationship-heavy-exploration` scenario requires repeated search-to-read navigation in
baseline mode and exercises the bounded planner in policy mode. Regression gates cover correctness,
steps, executed calls, and context size; shell-heavy and planner scenarios report before/after
metrics in the same result for direct comparison. Wall time is diagnostic only because shared CI
timing is unstable; RSS is measured by the external comparison wrapper, while unavailable in-process
probes are reported as zero.
