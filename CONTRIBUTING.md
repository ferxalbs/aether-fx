# Contributing to AETHER Fx

AETHER Fx is experimental. Keep changes small, explicit, and measurable.

Before opening a change, run:

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked
cargo test --workspace --all-features --locked
```

Do not add provider-specific inference code, unbounded tool output, terminal framework dependencies, telemetry, or licensing machinery. New dependencies need a documented reason, license, MSRV, and hot-path assessment in `docs/toolchain-baseline.md`.
