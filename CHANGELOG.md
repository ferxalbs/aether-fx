# CHANGELOG

All notable AETHER Fx changes are recorded here. This project is still in
foundational bootstrap, so the current work remains unreleased.

## Unreleased - 2026-08-21 - Foundational bootstrap and runtime hardening

> Status: unreleased foundational work. No artifact has been published.

### Added

#### Workspace and repository foundation

- Bootstrapped the Rust 2024 virtual workspace with exactly six crates:
  `aether`, `aether-core`, `aether-agent`, `aether-rainy`, `aether-tools`,
  and `aether-terminal`.
- Pinned Rust `1.98.0`, Cargo resolver `3`, the release profile, strict
  warning policy, rustfmt configuration, and dependency policy.
- Added Apache-2.0 licensing, minimal `NOTICE`, third-party notices,
  contribution guidance, security guidance, and repository operating rules.
- Added the CI and release workflow skeletons with locked builds, dependency
  policy, security checks, coverage, SBOM generation, checksums, and the six
  primary target matrix.
- Added local developer workflow skills and their lockfile, plus repository
  guidance for structure, testing, security, and release conventions.

#### Runtime boundaries

- Added domain primitives for IDs, events, typed errors, bounded output,
  workspace paths, permissions, sessions, model requests, and tool results.
- Added the backend-neutral agent loop, deterministic fake backend, session
  JSONL replay primitives, bounded event channels, tool continuation, and
  semantic session/turn/step identity.
- Added the `ModelBackend` abstraction and isolated the Rainy SDK behind
  `aether-rainy`.
- Added opaque provider continuation handling without exposing raw reasoning
  content.
- Added the shared cancellation flag and cancellation token used by agent,
  Rainy stream, tool, filesystem, and process work.

#### Exact v0.1 tool surface

- Added exactly nine model-visible tools: `read`, `list`, `find`, `search`,
  `write`, `patch`, `shell`, `process`, and `git`.
- Added typed schemas, permission classes, structured errors, bounded output,
  and registry validation for all nine tools.
- Added bounded batched file reads with line ranges, UTF-8/binary handling,
  and line numbers.
- Added ignore-aware directory listing and path/name finding.
- Added ripgrep-ecosystem content search with literal/regex modes, globs,
  context, hidden/binary policy, result limits, and bounded output.
- Added atomic writes with expected-content preconditions.
- Added strict multi-file patch validation, dry-run behavior, conflict
  reporting, preconditions, and best-effort rollback.
- Added direct-argv finite shell execution with bounded stdout/stderr,
  timeout, exit status, duration, and truncation metadata.
- Added persistent process lifecycle operations with stable process IDs,
  bounded buffered output, cancellation-aware read/write, status, and kill.
- Added structured read-oriented Git operations using the installed `git`
  executable without libgit2 or remote/destructive operations.

#### Authorization and terminal interaction

- Added `ExecutionPermit` binding authorization to the exact tool call, tool
  name, and permission class.
- Added `ToolExecutionContext` so every tool receives explicit authorization
  and cooperative cancellation state.
- Added permission brokers, session-level permission decisions, structured
  permission events, and a monochrome terminal permission prompt with
  allow-once, allow-session, and deny choices.
- Added the RAII terminal guard, raw input handling, Unicode-safe editing,
  ANSI/VT output, incremental rendering, monochrome styling, and Unix and
  Windows platform modules.
- Added blocking-operation isolation so filesystem and CPU-heavy tool work
  runs through `spawn_blocking` instead of occupying Tokio control tasks.

#### CLI and Rainy behavior

- Added `aether --version`, `--help`, `doctor`, `models`, prompt mode, and a
  minimal interactive shell.
- Added explicit model selection through `--model` or `AETHER_MODEL`; model
  selection fails closed instead of silently choosing a catalog position.
- Added multi-turn interactive continuation using opaque Rainy response IDs.
- Preserved the startup rule: the bare `aether` prompt appears without an
  immediate Rainy network request.
- Added explicit unsupported responses for capabilities that are not yet
  implemented rather than placeholder behavior.

#### Tests, benchmarks, and documentation

- Added unit coverage for path containment, symlink escape, permissions,
  cancellation, continuation identity, tool schemas, bounded outputs,
  patch conflicts, process behavior, terminal width, and Rainy event mapping.
- Added Criterion benchmarks for startup, parsing, rendering, event dispatch,
  file operations, listing, finding, searching, patches, session replay, and
  tool dispatch.
- Added the reproducible AETHER-versus-`rg` search comparison and documented
  its non-apples-to-apples process-startup caveat.
- Added architecture, security, Rainy integration, tool contract,
  performance, and toolchain baseline documentation.

### Changed

- Moved tool execution from an unqualified invocation to an explicit
  permission-and-cancellation context.
- Changed agent turns to preserve provider continuation across external turns
  while assigning fresh turn and step identities.
- Changed the interactive runtime to resolve permissions through the terminal
  broker and shut down persistent processes during cleanup.
- Changed process storage to shared handles with bounded output accounting and
  platform-specific process support.
- Changed terminal startup and prompt presentation to use the native
  monochrome shell substrate without an alternate screen.

### Security

- Kept all filesystem operations rooted in canonical workspace containment,
  including symlink escape checks.
- Kept writes, process execution, persistent processes, Git mutation, and
  outside-workspace access behind typed permission classes.
- Kept `RAINY_API_KEY` environment-only, secret-free in diagnostics, and out
  of sessions, tool output, and tracing.
- Kept provider-specific integrations out of AETHER; Rainy remains the only
  inference boundary.
- Kept telemetry, crash uploaders, licensing gates, activation, and remote
  mutation out of the bootstrap.

### Performance

- Kept tool output, search, process buffers, agent text, event queues, and
  session replay bounded.
- Kept public release builds free of `target-cpu=native` and configured fat
  LTO, one codegen unit, stripped symbols, and aborting panic.
- Recorded the current exploratory search benchmark in
  `docs/performance.md`; it is a measurement baseline, not a product claim.

### Known Limitations

- The verified Rainy SDK is `0.6.14`; the anticipated `6.5.x` generation was
  not available and has not been fabricated.
- Live Rainy integration tests remain opt-in and require `RAINY_API_KEY`.
- `aether resume` remains explicitly unsupported in this bootstrap phase.
- Full native validation for every release target belongs to CI; local
  cross-compilation is host-toolchain dependent.
- Release tooling is prepared in CI but no SBOM, attestation, or release
  artifact has been published by this work.
