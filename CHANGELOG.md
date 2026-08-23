# CHANGELOG

## Unreleased - 2026-08-23 (1) - Dependency-aware tool execution

> Status: unreleased work. No artifact has been published.

### Added

- Independent read-only tool calls now execute concurrently through a bounded scheduler,
  while workspace mutations, process effects, and conflicting footprints remain ordered.
- Typed tool-effect metadata, deterministic result ordering, cancellation coverage, and
  scheduler benchmarks were added.

## Unreleased - 2026-08-22 (11) - State-root and metadata initialization hardening

> Status: unreleased work. No artifact has been published.

### Security

- State-root overrides now require an absolute, physically resolved path outside the canonical
  workspace before any AETHER state is created; traversal components and workspace-directed
  intermediate links are rejected.
- Workspace metadata is staged as a private temporary file and atomically installed without
  replacement, so concurrent initialization validates one binding and incomplete temps cannot
  become `workspace.json`.

## Unreleased - 2026-08-22 (10) - Private application state and workspace isolation

> Status: unreleased work. No artifact has been published.

### Changed

- Persistent AETHER Fx application and session state now lives in OS application-state
  storage, never in a user workspace.
- Sessions are grouped under a deterministic BLAKE3 bucket for the canonical workspace,
  with bounded header-plus-tail discovery and workspace-scoped latest resume.
- `aether doctor` reports the canonical workspace, resolved state root, and resolved
  workspace session directory without exposing secrets.

### Security

- `AETHER_FX_STATE_DIR` is an explicit state-root override; default resolution follows
  platform application-state conventions and never falls back to the current directory.
- `workspace.json` binds each bucket to its canonical workspace, while state directories,
  metadata, JSONL, and compaction temps retain private permissions and no-follow containment.
- Repository-local `.aether` and `.aether-fx` layouts are unsupported and ignored without
  migration, scanning, or fallback.

### Documentation

- Documented the separation between repository data and private AETHER Fx application state,
  including platform-specific state-root resolution and the two independent boundaries.

## Unreleased - 2026-08-22 (9) - CLI state isolation and session reliability

> Status: unreleased work. No artifact has been published.

### Changed

- Persistent sessions use private OS application-state storage, while repository-local
  AETHER state is ignored completely.
- Bounded session summaries discard partial UTF-8 boundary records before decoding,
  preserving discovery of the latest committed turn across tail and crash boundaries.

### Security

- Session containment and no-follow protections cover the private state root, including
  workspace metadata, session JSONL files, and compaction temporaries.

## Unreleased - 2026-08-22 (8) - Contained session discovery and reliable resume

> Status: unreleased work. No artifact has been published.

### Changed

- Session listing reads bounded header and tail metadata instead of replaying complete JSONL
  history, while preserving newest-first ordering.
- `aether resume --latest` skips corrupt, empty, unsupported, or otherwise unrestorable sessions
  until it finds newest valid candidate.

### Security

- Session enumeration opens the selected workspace bucket, `sessions`, and JSONL entries
  through contained no-follow filesystem operations, rejecting links and reparse points.

## Unreleased - 2026-08-22 (7) - CLI and session workflow improvements

> Status: unreleased work. No artifact has been published.

### Added

- Added `aether sessions` and safe `aether resume --latest` discovery within the selected workspace.

### Changed

- One-shot and interactive prompts now use the same session-backed agent runtime path.

### Security

- Session replay accepts only already-contained descriptors; missing sessions are handled with typed errors.

All notable AETHER Fx changes are recorded here. The project remains
unreleased, with changes grouped by public capability.

## Unreleased - 2026-08-22 (6) - Session storage path hardening

> Status: unreleased work. No artifact has been published.

### Security

- State directories and JSONL files are created and opened without following symbolic
  links or Windows reparse points, and must remain inside the private state root.
- Compaction temps are created exclusively beside the selected OS-state session JSONL
  and cannot redirect writes through a planted symlink.

### Documentation

- Documented session-path containment, no-follow open/create semantics, and
  residual TOCTOU between validation and the final OS call.

## Unreleased - 2026-08-22 (5) - Session and context safety hardening

> Status: unreleased work. No artifact has been published.

### Changed

- Session JSONL is schema version 3. Each completed turn is one committed
  `TurnSnapshot` record. Truncated trailing records are ignored; previous
  committed turns remain valid.
- Persistence stores session identity, workspace, model, turn metadata, file
  path/hash/range metadata, safe tool summaries, and sanitized continuation.
  Raw prompts, assistant text, excerpt bodies, and shell/process/git output
  are omitted by construction.
- Resume discards Rainy `previous_response_id` when the workspace root differs
  or inspected files are stale or missing, then reconstructs from bounded local
  context.
- Continuation recovery uses typed `BackendError::InvalidContinuation` instead
  of matching user-facing error strings.

### Security

- Unix session directories are mode `0700` and session JSONL/temp files are
  `0600`. `payload_contains_secrets` remains defense-in-depth after record
  minimization and is not complete secret detection.
- Compaction refuses to rewrite a session after corrupt replay, leaving the
  original file untouched.

### Documentation

- Documented persisted versus omitted session fields, Unix session modes, turn
  commit/recovery, continuation invalidation, and the Rainy SDK 0.6.14 mapping
  limitation for continuation errors.

## Unreleased - 2026-08-22 (4) - Context engine and session continuity

> Status: unreleased work. No artifact has been published.

### Added

- Added a bounded workspace context engine that tracks the current task,
  inspected and modified files, targeted excerpts, compact tool summaries, git
  state, and sanitized Rainy continuation without resending full files or raw
  tool history on every model step.
- Activated local versioned JSONL sessions in private application-state storage and
  implemented `aether resume <session-id>` with workspace, schema, path, and stale-hash
  validation.
- Added deterministic local tool-result compaction and continuation
  reconstruction from bounded persisted state when Rainy `previous_response_id`
  is unusable.

### Changed

- New-file `create_only` installation prefers native exclusive rename
  (Unix `renameat_with(NOREPLACE)`, Windows `MoveFileExW` without replace) and
  falls back to hard links only when exclusive rename is unavailable.

### Security

- Session persistence refuses secret-bearing records and stores only opaque
  continuation identity keys. `create_only` still refuses to replace an
  existing destination.

### Performance

- Re-ran the existing Criterion suite. Small-file read and session replay
  are slower in this configuration because `read` now hashes content and
  session JSONL validates schema v2; remaining statistically flagged
  changes are recorded in `docs/performance.md`.

### Documentation

- Documented context/session bounds, resume behavior, and remaining
  filesystems that lack both exclusive rename and hard links.

### Known Limitations

- Filesystems without exclusive no-replace rename or hard links cannot
  atomically create-if-absent; AETHER fails closed rather than copying over an
  existing destination.
- Git awareness remains read-only; working-tree mutation is still out of
  scope.

## Unreleased - 2026-08-22 (3) - Runtime correctness and concurrency hardening

> Status: unreleased work. No artifact has been published.

### Fixed

- Closed same-path AETHER `write`/`patch` mutation races with weak-reference
  per-destination coordination and deterministic multi-file lock ordering.
- Made `create_only` use commit-time no-replace installation and strengthened
  filesystem precondition revalidation before staged commits.
- Preserved persistent-process tracking until termination is confirmed and
  surfaced kill, wait, timeout, and session-cleanup failures.
- Made Ctrl+C during permission approval cancel the active turn, clean pending
  broker state, and remain distinct from ordinary denial.
- Rejected Rainy streams that end without a valid terminal response event and
  prevented truncated tool calls from executing.

### Changed

- Updated the six-target CI matrix to execute native workspace tests in
  addition to centralized quality and policy gates.
- Made process drain-task ownership and bounded five-second termination waits
  explicit in the runtime behavior.

### Security

- Strengthened mutation precondition handling and process-lifecycle failure
  reporting without claiming cross-process compare-and-swap guarantees.

### Performance

- Re-ran the existing benchmark suite and recorded current local measurements
  and observed regression signals in `docs/performance.md`.

### Documentation

- Documented atomic replacement versus atomic create-if-absent, optimistic
  content preconditions, process cleanup aggregation, permission cancellation,
  native CI execution, and Rainy terminal-stream handling.

### Known Limitations

- An arbitrary external process can still mutate the filesystem namespace in
  the final window after AETHER revalidation and before the atomic OS call;
  same-path AETHER races are serialized and the limitation is documented.

## Unreleased - 2026-08-22 (2) - Native runtime hardening and interactive agent

> Status: unreleased work. No artifact has been published.

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
