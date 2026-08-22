# CHANGELOG

All notable AETHER Fx changes are recorded here. The project remains
unreleased, with changes grouped by implementation phase.

## Unreleased - 2026-08-22 (2) - Phase 1 native runtime hardening and interactive agent

> Status: unreleased Phase 1 work. No artifact has been published.

### Added

#### Cooperative runtime control

- Added the std-only `CancellationFlag` in `aether-core`, with agent-level
  notification support for interruptible async waits.
- Added `ToolExecutionContext` so every tool receives cancellation state and
  an authorization permit.
- Added cancellation checkpoints to filesystem reads, directory traversal,
  search matching, writes, patches, shell execution, persistent process I/O,
  and model streaming.
- Added a deterministic current-thread regression test proving blocking tool
  work does not monopolize the Tokio control plane.

#### Safe filesystem replacement

- Added a platform-aware atomic write path using same-directory temporary
  files, collision-resistant PID/counter names, exclusive creation, flush,
  file synchronization, cleanup, and precondition checks.
- Added the Windows `ReplaceFileW` implementation through `windows-sys 0.61.2`
  without deleting the destination before replacement.
- Added Unix executable-permission preservation tests and native replacement
  coverage for existing and new files.
- Added staged, rollback-capable multi-file patch commits with explicit
  high-severity errors when rollback is incomplete.

#### Persistent process runtime

- Added a bounded process registry with a maximum of 16 session-owned
  persistent processes.
- Added independent per-process stdin, stdout, stderr, and child-state
  synchronization so process I/O does not hold a global registry lock.
- Added continuous stdout/stderr drain tasks backed by 256 KiB per-stream
  bounded buffers, recent-output retention, and `dropped_bytes` metadata.
- Added session shutdown cleanup and cancellation-aware process start, read,
  write, signal, kill, and status behavior.

#### Interactive permissions

- Added backend-neutral `PermissionRequested` and `PermissionResolved`
  events.
- Added allow-once, allow-session, and deny decisions through a monochrome
  terminal prompt.
- Added session grants scoped by tool and permission class.
- Added permits bound to the exact call ID, tool name, and permission class.
- Added structured operation details for writes, patches, shell commands, and
  persistent process operations without including file bodies or secret
  environment values.
- Added fail-closed behavior for mutation when no interactive terminal is
  available.

#### Multi-turn interactive session

- Changed bare `aether` into a persistent prompt loop that reuses the agent,
  backend, tool registry, process registry, permission grants, and session
  identity.
- Added stable session IDs and unique monotonic turn IDs.
- Added opaque Rainy continuation handoff between user turns.
- Added Ctrl+C turn cancellation that returns control to the interactive
  prompt while preserving established session processes.
- Preserved local prompt display before backend construction or Rainy network
  activity.

### Changed

#### Tool execution and bounds

- Moved potentially blocking filesystem, canonicalization, hashing, traversal,
  and search work to bounded `spawn_blocking` operations while retaining the
  current-thread Tokio scheduler.
- Kept shell execution as direct argv spawning and Git operations read-only.
- Kept ordinary tool failures and permission denials as structured tool
  results for model recovery instead of terminating the whole turn.
- Kept unknown tool calls fail-closed with `unknown_tool`.
- Retained exactly nine model-visible tools: `read`, `list`, `find`, `search`,
  `write`, `patch`, `shell`, `process`, and `git`.

#### Deterministic Rainy selection

- Changed model selection to `--model`, then `AETHER_MODEL`, then a clear
  configuration error.
- Removed catalog-order fallback and unnecessary catalog fetches from normal
  model-selected inference.
- Kept `rainy-sdk 0.6.14`; no unpublished Git dependency or speculative SDK
  upgrade was introduced.

### Tests

- Added coverage for atomic replacement, temporary cleanup, precondition
  mismatch, Unix executable permissions, staged patch rollback, typed
  cancellation, traversal cancellation, model-stream cancellation, process
  concurrency, process draining, bounded output, process limits, permission
  decisions, permit binding, continuation identity, stable sessions, model
  precedence, and no-network startup.
- Added large shell output, large persistent-process output, and large
  workspace bounds coverage.
- Added a registry assertion that exactly nine model-visible tools exist.

### Documentation

- Updated architecture documentation for the blocking worker boundary,
  process subsystem, cancellation, permission broker, and nine-tool executor.
- Updated security documentation for atomic replacement, workspace
  containment, permits, session grants, process ownership, cleanup, bounded
  output, cancellation, and non-interactive fail-closed behavior.
- Updated tool contracts with process buffer and truncation metadata.
- Updated Rainy integration, toolchain baseline, and performance documentation.

### Dependencies

- Enabled the existing `windows-sys 0.61.2` `Win32_Storage_FileSystem`
  feature for `ReplaceFileW`.
- Added direct `serde_json` use to the terminal permission renderer and the
  Windows-targeted `windows-sys` edge to `aether-tools`.
- Added no new crate package and kept the existing Rust `1.98.0` toolchain.

### Performance

- Added benchmarks for blocking dispatch, cancellation checks, permission
  decisions, multi-turn bookkeeping, atomic writes, staged patches, process
  buffer reads, and process registry lookup.
- Recorded the current release binary size as 4,943,824 bytes (4.71 MiB).
- Recorded 225 resolved lockfile packages; no new package dependency was
  introduced.
- No unsupported before/after speedup claim is made where no pre-Phase 1
  baseline was available.

### Known Limitations

- Native Windows execution and the complete six-target CI matrix remain
  pending external CI verification.
- Full `aether resume` remains unsupported; in-process multi-turn continuity
  is the supported Phase 1 behavior.
- Extended attributes, ACLs, and alternate data streams are not claimed as
  preserved by atomic replacement.
- PTY support, Git mutations, shell interpreter semantics, and live Rainy
  probing remain out of scope for this phase.

## Unreleased - 2026-08-21 (1) - Foundational bootstrap and runtime hardening

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
