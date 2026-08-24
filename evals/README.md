# Deterministic coding-agent evaluations

Run the offline deterministic suite with:

```text
cargo run --release -p aether-eval -- target/aether-eval-results.json
```

Each task declares its prompt, file outcomes, exact verification command, model-step budget,
tool-call budget, and verification-attempt budget in `crates/aether-eval/src/lib.rs`. Fixtures are
small standalone Rust workspaces. The runner copies one into a new temporary directory for every
task and removes the copy afterward, so runs never mutate fixture sources or reuse build state.

The default backend is scripted and deterministic. It performs no provider or network calls and
sets Cargo offline for fixture verification. `EvalBackend` is the integration boundary for real
providers; provider-backed evaluations should use a separate binary or feature so default CI remains
offline and reproducible.

JSON output includes per-task correctness, wall time, harness overhead, model steps, requested and
executed tool calls, prevented redundant calls, bytes read, context bytes, verification attempts,
and final test status. Regression gates cover correctness, steps, executed calls, and context size.
Wall time is diagnostic only because shared CI timing is unstable.
