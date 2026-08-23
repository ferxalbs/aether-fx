# Repository Guidelines

## Project Structure & Module Organization

AETHER Fx is a Rust 2024 Cargo workspace. The binary entry point is
`crates/aether/src/main.rs`; reusable code is split into focused crates:
`aether-core` (shared types and contracts), `aether-agent` (backend-neutral turn
loop), `aether-rainy` (the only Rainy SDK adapter), `aether-tools` (nine bounded
local tools), and `aether-terminal` (input, rendering, and platform code).
Most tests live inline beside the implementation under `crates/*/src`.
Criterion benchmarks are in `crates/aether-tools/benches/tools.rs`; design,
security, and performance notes are in `docs/`.

## Build, Test, and Development Commands

The repository pins Rust 1.98.0 in `rust-toolchain.toml`.

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo nextest run --workspace --all-features --locked
cargo run -p aether -- --version
cargo bench -p aether-tools --bench tools
```

Run formatting, check, clippy, and tests before opening a PR. CI also runs
`cargo deny check`, `cargo audit`, and `cargo llvm-cov` to produce `lcov.info`.

## Coding Style & Naming Conventions

Use Rustfmt with four-space indentation, edition 2024, and the repository’s
100-column width (`rustfmt.toml`). Use `snake_case` for functions, modules, and
tests; `CamelCase` for types and traits. Workspace lints deny warnings,
`unsafe`, `dbg!`, and `todo!`. Keep provider-specific types inside
`aether-rainy`, and document any new dependency’s purpose, license, MSRV, and
hot-path impact in `docs/toolchain-baseline.md`.

## Testing Guidelines

Name tests after observable behavior, such as
`default_policy_rejects_mutation_and_path_escape`; use `#[tokio::test]` for
async behavior. Add boundary and failure-case coverage for filesystem,
process, permissions, path containment, and bounded output changes. CI records
workspace coverage but defines no minimum threshold. Do not use live Rainy or
network calls in tests or benchmarks.

## Commit & Pull Request Guidelines

Recent commits use concise Conventional Commit-style subjects such as
`feat: add ...`. Follow that pattern (for example, `fix: reject escaped paths`)
and keep each commit focused. PRs should explain the behavior change and
security impact, list validation commands run, link an issue when applicable,
and include documentation or configuration updates when relevant. Ensure all
CI checks pass before requesting review.

## Changelog and release format

Keep `CHANGELOG.md` history-only. Every change belongs under one dated entry
at the top of the file:

```markdown
## Unreleased - YYYY-MM-DD (x) - Short description
```

Before adding a new versioned release, use this exact heading format:

```markdown
## vX.Y.Z - YYYY-MM-DD (x) - Short release tagline
```

Release rules:

1. Use a lowercase `v` and a complete semantic version, such as `v0.1.0`.
2. Use the ISO-8601 date format `YYYY-MM-DD`.
3. Use `x` as the sequential entry number for that date: start at `(1)`,
   increment to `(2)`, `(3)`, and so on for each additional version or
   unreleased entry on the same date, then reset to `(1)` on the next date.
4. Add one concise, outcome-focused tagline after the date and sequence.
   Describe the public behavior change. Do not use internal engineering-phase
   names such as `Phase 2.1` or `Phase 3` as the heading tagline. Prefer
   titles such as `Session storage hardening` or `Native runtime reliability` but describe the work in the session and based on the changes not static or similar entrys.
5. Organize each entry under only the sections that apply: `Added`,
   `Changed`, `Fixed`, `Security`, `Performance`, `Documentation`, and
   `Known Limitations`.
6. Keep all entries in the single public `CHANGELOG.md`. When a release is
   prepared, move its completed items into the new version heading and create
   the next dated `Unreleased` entry with the correct sequence number.
7. Do not add a versioned heading for an unpublished, untagged, or unvalidated
   release. Validate the release before recording it as a versioned entry.
8. Multiple commits and pull requests are grouped into the relevant changelog
   entry; the sequence number tracks public changelog entries or releases,
   not individual commits. AETHER Fx is a public open-source project, so
   `CHANGELOG.md` is the single clear public record of that history.

## Security & Configuration

Never commit credentials. `RAINY_API_KEY` is read from the environment only
when inference is requested and must not be printed or persisted. Preserve the
workspace containment, typed-permission, and bounded-output guarantees; review
`SECURITY.md` and `docs/security-model.md` when modifying tools or process
execution.
