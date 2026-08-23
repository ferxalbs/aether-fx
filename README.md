# AETHER Fx

AETHER Fx is an experimental, native terminal coding agent written in Rust. It is one process, one binary, one async runtime, and a deliberately small set of local tools. Rainy owns inference; AETHER owns the local environment and terminal.

## Status

This repository is in foundational bootstrap. The workspace, runtime boundaries, terminal substrate, nine-tool registry, context engine, local JSONL sessions, deterministic fake backend, and Rainy SDK adapter are being established. Incomplete capabilities return explicit errors rather than pretending to work.

## Why native Rust

AETHER needs bounded local filesystem/process behavior, predictable startup, terminal restoration, cross-platform OS boundaries, and a small dependency surface. Rust provides those primitives without a WebView, JavaScript runtime, or TUI framework.

## Model-visible tools

The v0.1 registry contains exactly: `read`, `list`, `find`, `search`, `write`, `patch`, `shell`, `process`, and `git`.

## Supported release targets

The release workflow treats Windows x64 and ARM64, macOS Intel and ARM64, and Linux x64 and ARM64 GNU as first-class targets. The ARM64 Windows and Ubuntu runners are currently preview labels and are called out in the workflow.

## Build

Rust 1.98.0 is pinned in `rust-toolchain.toml`.

```text
cargo check --workspace --all-targets --all-features --locked
cargo test --workspace --all-features --locked
cargo run -p aether -- --version
```

`aether` does not contact Rainy at startup. Inference requires `RAINY_API_KEY`; the key is read from the environment only when an inference operation is requested and is never printed or persisted. Repositories contain user/project data; AETHER Fx persistent application and session state lives in private OS state storage, grouped by canonical workspace identity. Set `AETHER_FX_STATE_DIR` for tests or controlled environments. Resume with `aether resume <session-id>`.

## Security and licensing

AETHER operates local tools against a canonical workspace root, bounds model-visible output, and keeps permissions separate from rendering. See `docs/security-model.md`. The project is pure Apache-2.0 open source; there is no activation, premium binary, telemetry, or required account to execute the binary.

## License

Apache-2.0. See [LICENSE](LICENSE).
