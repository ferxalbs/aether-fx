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

The capability suite also runs the tracked Git-backed `control-loop` fixture at
`evals/fixtures/control-loop/notes.txt`. Its serialized control evaluation contains two separate
controls: a positive runtime-owned verification case and a negative premature-completion case.
The positive case allows at most one additional optimized execution for a fresh runtime
verification; a non-read or dependency-ambiguous observation is not treated as reusable without
current freshness evidence. The negative case passes only when both traces reject completion
before required work, verification, or mutation success is established.

Capability output is separate from efficiency and regression output. It reports success@1,
intentionally interrupted baseline success, newly solved tasks, checkpoint and WorkState survival,
multi-file correctness, typed recovery, stale-evidence rejections, user-change corruption,
verification attempts/failures, model requests, executed tools, process spawns, context and
model-visible bytes, wall time, and unresolved work at finish. The offline suite uses one fixed
deterministic trial per task; provider-backed repeated trials remain a separate integration concern
behind `EvalBackend` and are not fabricated by the offline runner.

Each task declares its prompt, file outcomes, exact verification command, model-step budget,
tool-call budget, and verification-attempt budget in `crates/aether-eval/src/lib.rs`. The suite
also includes a shell-heavy command-reuse scenario with repeated direct `cargo metadata`,
`cargo locate-project`, and `cargo tree` argv calls so command intelligence can be measured
without shell parsing or network access.
Fixtures are
small standalone Rust workspaces. The runner copies one into a new temporary directory for every
task and removes the copy afterward, so runs never mutate fixture sources or reuse build state.

The default backend is a deterministic offline trace replay, but each trace is executed by the
production `Agent`, policy, scheduler, `ContextEngine`, nine-tool registry, and real fixture
filesystem. It performs no provider or network calls and sets Cargo offline for fixture
verification. `EvalBackend` remains the trace-generation boundary for real providers; provider-
backed evaluations should use a separate binary or feature so default CI remains offline and
reproducible.

The production loop also promotes bounded source windows from compiler/test diagnostics and
successful mutations. A subsequent exact read can be served from that current evidence only after
a live content-hash recheck; stale or partial evidence falls back to the real tool. Recovery
updates are capped separately from the full context packet so the optimization does not trade
fewer tool/model turns for unbounded prompt growth.

The focused orchestration coverage is split between the real capability replay and deterministic
component tests. It covers compiler diagnostics with automatic source hydration, post-mutation
changed-region refresh, external stale evidence, patch-conflict and environment taxonomy,
multi-file WorkingSet ranking, nested scoped-instruction precedence, targeted-to-package
verification ordering, public/workspace-dependent verification escalation, fail-closed completion
for stale verification and unsatisfied work, dirty user-owned path preservation, checkpoint
resume, and task-revision invalidation. The capability replay runs against copied fixture
repositories; the component cases assert bounded runtime state and do not mock successful
completion.

JSON output includes per-task correctness, model requests, tokens when the provider reports them,
requested and executed calls, prevented/reused observations, bytes read, bytes shown to the model,
context bytes, static-prefix/tool-schema/context/tool-result/policy-feedback/protocol byte
categories, process spawns, automatic evidence reactions, diagnostic hydration, stale invalidations,
verification escalation, completion rejections, verification attempts, wall/provider/local time,
and platform probes for RSS and allocations. Each task also contains a bounded trajectory report that classifies
repeated observations, search-to-read chains, verification recovery, and mutation-before-
inspection candidates. The aggregate report merges those pattern counts for the baseline traces.
The `relationship-heavy-exploration` scenario requires repeated search-to-read navigation in
baseline mode and exercises the bounded planner in policy mode. Regression gates cover correctness,
steps, executed calls, and context size; shell-heavy and planner scenarios report before/after
metrics in the same result for direct comparison. Wall time is diagnostic only because shared CI
timing is unstable; RSS is measured by the external comparison wrapper, while unavailable in-process
probes are reported as zero.
