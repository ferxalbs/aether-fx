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

The default backend is scripted and deterministic. It performs no provider or network calls and
sets Cargo offline for fixture verification. `EvalBackend` is the integration boundary for real
providers; provider-backed evaluations should use a separate binary or feature so default CI remains
offline and reproducible.

JSON output includes per-task correctness, wall time, harness overhead, model steps, requested and
executed tool calls, prevented redundant calls, bytes read, context bytes, verification attempts,
verification scope and quality, planner attempts/hits, ambiguity aborts, planned actions, planner
bytes, and final test status. The `relationship-heavy-exploration` scenario requires repeated
search-to-read navigation in baseline mode and exercises the bounded planner in policy mode.
Regression gates cover correctness, steps, executed calls, and context size; shell-heavy and
planner scenarios report before/after metrics in the same result for direct comparison.
Wall time is diagnostic only because shared CI timing is unstable.
