# CHANGELOG

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## v0.1.0-alpha-06 - 2026-09-03 (1) - Inspectable WebMCP browser workflow

### Added

- Added the separate `aether-web` Rust/WASM crate and static `web/` vertical slice. The hosted
  page contains a deterministic four-file repository, active task, source evidence, proposed
  diff, verification checks, evidence ledger, tool trace, and visible human Accept/Reject gate.
- Added six provider-side WebMCP tools: `inspect_workspace`, `read_file`, `search_workspace`,
  `propose_patch`, `verify_patch`, and `get_task_state`. Visible controls and WebMCP callbacks use
  the same bounded in-memory runtime and workflow state.
- Added the opt-in `aether-obscura` boundary with a static Obscura v0.2.1 manifest, explicit
  installation consent, SHA-256/size/version verification, atomic activation, private
  workspace-scoped provider state, and one supervised MCP `stdio` session.
- Added the six fixed native `browser.*` tools, `BrowserRead`/`BrowserAction` permission classes,
  `BrowserSession` scheduler footprints, and the read-only `ExternalResearch` turn mode.
- Added `/browser`, `/browser status`, `/browser stop`, and `/browser update` lifecycle commands;
  `/browser <task>` can perform bounded web research without repository mutation or repository
  evidence requirements.

### Changed

- Bumped the workspace release identity to `0.1.0-alpha-06` and kept the native terminal, nine
  native tools, Rainy adapter, workflow safeguards, and optional Obscura path intact.
- Updated the provider boundary to `rainy-sdk 0.6.51` with `default-features = false`; the
  existing typed Responses and Chat Completions streams remain in use.
- Browser tools are registered only after a validated native provider starts; the native inactive
  model surface remains exactly nine tools. Provider schemas and descriptions cannot expand it.
- Added pinned WASM/static bundle checks to CI and release preflight. Vercel publishes `web/dist`
  without a server, native bridge, or runtime secret.
- `/clear`, `/exit`, and normal session shutdown stop Obscura while retaining its private profile;
  `/resume` does not reanimate the provider or restore active tabs.

### Fixed

- Fixed Windows compilation of the optional Obscura installer under the workspace's denied-warning
  policy.
- Models without confirmed tool support remain visible but cannot be selected for the coding
  shell.
- Rainy `TOOLS_NOT_ALLOWED` responses are treated as non-retryable model incompatibilities,
  remembered for the current process, and surfaced as unavailable on the next model selection;
  interactive sessions remain open so another model can be chosen.

### Security

- The WebMCP surface validates strict `additionalProperties: false` schemas in JavaScript and Rust,
  bounds requests/results/files/lines, rejects path escapes and secret-like patch markers, and
  keeps `accept_patch`/`reject_patch` outside the model-visible tool set.
- The browser fixture cannot read the checkout, spawn a process, contact Rainy or Obscura, persist
  credentials, or execute arbitrary JavaScript. Unsupported WebMCP hosts receive a compatibility
  notice and never gain a fallback capability.
- Added bounded newline-delimited MCP framing, fixed-schema validation, stderr draining, timeout
  and cancellation invalidation, direct spawn without a shell, cleared provider environment, and
  fail-closed handling for unsupported server messages and dynamic tool-list changes.
- Added HTTP(S) URL and credential checks, private/link-local/metadata literal-address rejection,
  status-only browser persistence, and a fixed internal performance evaluator with no arbitrary
  JavaScript or generic MCP passthrough.

### Performance

- Kept the WebMCP page free of a frontend framework and kept its demo data in bounded in-memory
  Rust/WASM state; the native binary does not link the browser crate.
- Kept Obscura, its browser engine, and its network cost on the cold optional path; one provider
  process and one serialized browser session are reused across turns rather than started per call.

### Documentation

- Documented the WebMCP boundary, local WASM/static build, compatibility behavior, public hosting
  handoff, prior work versus challenge work, submission text, and short demo script.
- Documented the Obscura v0.2.1 artifact manifest, release licensing, fixed MCP mapping, lifecycle,
  network limitation, persistence boundary, and separate AETHER/provider performance accounting.

### Known Limitations

- The WebMCP API and browser support are experimental. The hosted slice demonstrates a fixed
  in-memory repository rather than arbitrary local checkout access or Cargo execution; a verified
  public Vercel URL and challenge video still require the release handoff step.
- Obscura v0.2.1 publishes no Windows aarch64 asset and its safe default has no loopback-only
  allowlist. AETHER therefore supports the five published targets and public HTTP/HTTPS only;
  localhost auditing awaits a narrow upstream contract.

## v0.1.0-alpha-05 - 2026-09-02 (4) - Protocol-aware model workflow

### Changed

- Added explicit model transport visibility to the selector, status output, and model-switch
  confirmation so the active model name, ID, and request protocol stay readable together.
- Added a backend-neutral bounded conversation history so Chat Completions turns can replay
  assistant tool calls and tool results without exposing provider-specific types to the agent.

### Fixed

- Routed OpenAI models through Responses and other Rainy model providers through Chat Completions,
  including OpenAI-compatible tool schemas, fragmented tool-call assembly, usage, and terminal
  handling.
- Added model and transport context to Rainy operation failures so errors such as
  `TOOLS_NOT_ALLOWED` identify what was actually attempted.

## v0.1.0-alpha-04 - 2026-09-02 (2) - Model-first terminal workflow

### Changed

- Redesigned the interactive model picker so model names are the primary identity, with the stable
  model ID and capability summary visible beneath each entry, current selection state, filtering,
  unavailable-model feedback, and quick-select controls.
- Updated human-readable catalog, status, and turn summaries to keep the active model identity
  visible throughout the session.

### Fixed

- Made concurrent workspace state initialization collision-safe on Windows by retrying uniquely
  sequenced temporary metadata files.

## v0.1.0-alpha-03 - 2026-09-01 (1) - Native self-update and terminal reliability

### Added

- Added adaptive tool surfaces, targeted context selection, progress/stall awareness, bounded
  recovery feedback, and runtime-owned evidence and completion review for coding tasks.
- Added the native AETHER Fx interactive shell with local `/` discovery and `/model`, `/status`,
  `/context`, `/config`, `/auth`, `/sessions`, `/resume`, `/doctor`, `/help`, `/clear`, and
  `/exit` commands.
- Added the explicit `aether update` and `/update` commands with stable/prerelease SemVer
  discovery, target-specific direct binaries, stable JSON output, and offline update capability
  diagnostics in `aether doctor`.
- Added live Rainy model catalog presentation with capability, access, context, reasoning,
  modality, and data-policy metadata, plus deterministic model selection and safe continuation
  invalidation on model changes.

### Changed

- Unified interactive, one-shot, piped, JSON, and headless execution behind explicit terminal
  boundaries; local slash commands remain local and never reach the model.
- Added HTTPS plus SHA-256 manifest verification, bounded streaming downloads, same-filesystem
  atomic executable replacement, temporary update locking, and Windows handoff behavior without
  elevation, persistent cache, permanent backups, telemetry, or a permanent helper.
- Expanded each GitHub prerelease with a directly downloadable binary asset while retaining the
  existing archive, SBOM, installer, checksum, and provenance contracts.
- Migrated the provider boundary to `rainy-sdk 0.6.50` with `default-features = false`, typed
  Responses streaming, a single SDK-owned bounded SSE parser, typed reasoning semantics, and no
  obsolete adapter compatibility path.

### Fixed

- Completion now remains blocked by stale or missing evidence, unresolved work, incomplete
  verification, and unsuccessful mutations; session checkpoints and resume preserve the current
  task state across interruption.
- Rainy stream handling rejects incomplete terminal frames, maps typed SDK failures to bounded
  backend errors, and keeps inference send-once at the adapter boundary.

### Security

- Preserved expected-hash mutation checks, dirty-user-change preservation, fail-closed
  noninteractive permissions, and private session/configuration state.
- Kept API keys out of normal configuration, history, sessions, logs, JSON output, and model
  context; credential helpers and editors run without a shell, and remote catalog data is
  terminal-sanitized and bounded.
- Kept the updater fixed to the official repository, without authorization headers or configurable
  mirrors; it rejects links/reparse points, read-only or non-regular destinations, unverified
  assets, malformed checksums, concurrent update attempts, and version mismatches.

### Performance

- The locked stripped macOS release binary measured 6,412,584 bytes (6.12 MiB) after adding the
  explicit updater; deterministic evaluation retained the frozen regression, capability, and
  control-loop gates while reducing model-visible runtime overhead and tool-schema overhead.

### Documentation

- Updated installation, interactive/headless usage, model/authentication behavior, Rainy SDK
  integration, self-update behavior, security boundaries, dependency notices, performance
  evidence, and release packaging documentation for the alpha-03 snapshot.

## v0.1.0-alpha-02 - 2026-08-29 (4) - Adaptive orchestration and evidence-fresh completion

### Release packaging and installation

#### Changed

- Published the second installable alpha contract with `aether --version` reporting
  `0.1.0-alpha-02`; installers and GitHub Release archives remain version-pinned to this tag.

#### Fixed

- Release packaging generates the CycloneDX binary SBOM with `--describe binaries` and renames
  the `aether_bin.cdx.json` output to `bom.json`. `--override-filename` cannot be combined with
  `--describe` in cargo-cyclonedx 0.5.9.
- The GitHub prerelease job authenticates `gh` with `GH_TOKEN` from `github.token`.

### Fixed

- Automatic live-source refresh stores portable `/` inspected paths, so Windows known-shell
  mutations refresh `src/lib.rs` instead of leaving it stale beside a native `src\lib.rs` entry.

### 2026-08-29 — Evidence freshness and completion reliability

### Changed

- Successful non-read discovery and verification observations are re-executed unless their full
  dependency freshness can be proven; this avoids returning stale search, listing, finding, or
  verification results from the generic observation cache.
- Exact read observations remain reusable when a bounded live hash of the canonical file still
  matches the retained evidence. Known and certain shell mutations may clear stale workspace
  state only after every affected path has been refreshed successfully; uncertain or partial
  refreshes remain stale.
- Installed verification plans normalize every step to `pending`, and completion requires an
  observed successful verification for the current workspace revision.
- The conservative re-execution rule can spend an extra tool call when dependency freshness is
  uncertain; that bounded cost is preferred to speculative reuse of stale repository state.

### Added

- Added a deterministic negative premature-completion trajectory proving that a finish attempt
  without required work or observed verification is rejected in both baseline and optimized
  control runs.

### Security

- No permission, workspace-containment, canonical-path, symlink/reparse, permit, or expected-hash
  boundary is weakened. Freshness decisions only control whether an observation may be reused;
  they never grant mutation authority.

### 2026-08-29 — Runtime-owned completion and verification

### Added

- Added runtime-owned Discover → Understand → Implement → Recover → Verify → Review → Done
  orchestration with bounded checkpoint restoration, operational WorkingSet projections, and
  deterministic completion review.
- Added mutation-triggered scoped verification through the existing permission and scheduler
  path, plus a Git-backed control-loop evaluation for correctness-preserving zero-waste execution.

### Changed

- Task steering now advances revisions when acceptance constraints change and reopens incompatible
  completion state while preserving valid repository evidence.
- Read-cache reuse now validates live canonical file hashes and rejects symlinked or out-of-root
  files; bounded repeated-failure suppression remains available for safe recovery.
- Verification classification now recognizes `git diff --check`, and persisted workflow metrics
  remain available when an agent turn returns a typed error.

### Security

- Automatic verification continues through normal action preparation, permission gates, scheduling,
  and execution; completion remains fail-closed on stale evidence, unresolved work, or incomplete
  verification.

### 2026-08-29 — Adaptive repository orchestration

### Added

- Added a bounded seven-phase Turn Director, operational WorkingSet projection, typed Recovery
  Controller state, current task revisions, and fail-closed completion review.
- Added scope-aware verification escalation for public Rust changes in multi-package workspaces,
  including required dependent checks after targeted/package verification.
- Added deterministic orchestration coverage and before/after metrics for evidence reactions,
  diagnostic hydration, mutation refresh, stale invalidation, verification escalation, and
  completion rejection.

### Changed

- User steering can update bounded task constraints without discarding still-valid repository
  evidence; changed task revisions invalidate incompatible completion and verification claims.
- Compiler/test failures, patch conflicts, stale evidence, environment failures, and permission
  failures now select distinct bounded recovery behavior while preserving model-owned repair
  decisions.

### Security

- Completion requires current task, evidence, verification, blocker, and user-ownership facts;
  stale or missing state cannot authorize a mutation result or Done state.

### 2026-08-28 — Evidence-driven coding loop hardening

### Added

- Added bounded live source promotion around compiler/test diagnostics and successful mutations,
  so recovery steps receive the affected paths and a small current source window.
- Added an intent-only `<aether-work>` outline channel for bounded subgoals, dependencies, and
  paths; runtime observations still own evidence, verification, blockers, and completion.

### Changed

- Exact current read observations can be reused after a successful mutation only when the live
  canonical file re-hashes to the retained content hash; stale, partial, and ambiguous reads
  fall back to the read tool.
- Context selection now supplies effective instruction bodies broad-to-specific for active paths
  and excludes unrelated instruction previews from repository metadata.

### Security

- Automatic evidence reads are bounded, canonical-root-contained, symlink-rejecting observations
  and never grant mutation authority or bypass tool permits and expected-hash checks.

### 2026-08-27 — Native Windows workspace paths

### Fixed

- Workspace-relative paths now compare and display with portable `/` separators, so Windows
  inspection, writes, and eval replay no longer treat `src\lib.rs` and `src/lib.rs` as different
  files.
- Persistent-process overflow coverage dumps a large file on Windows instead of a slow `cmd` FOR
  loop, so ARM runners still fill the 256 KiB drain buffer and report dropped bytes.

### Changed

- Full deterministic and capability eval suites run once from the `aether-eval` binary in CI
  instead of being re-executed as nested `cargo test` work on every native target.

## v0.1.0-alpha-01 - 2026-08-27 (1) - First installable alpha prerelease

### Release packaging and installation

#### Added

- Added version-pinned macOS/Linux and Windows installers, six native GitHub Release archives,
  aggregate SHA-256 verification, CycloneDX SBOMs, and build provenance attestations.

#### Changed

- Published the first installable alpha contract with `aether --version` reporting
  `0.1.0-alpha-01`; Rainy SDK remains compiled into AETHER and is not a separate installation.

#### Documentation

- Documented the alpha install, configuration, model selection, manual archive installation,
  checksum verification, supported platforms, and offline `aether doctor` setup diagnostics.

### 2026-08-26 — Long-horizon checkpoint recovery


#### Added

- Added compact adaptive `WorkState` for complex tasks, including bounded objectives, acceptance
  criteria, path subgoals, dependencies, evidence, mutations, verification records, and blockers.
- Added resumable `Checkpoint` session records for cancelled and incomplete turns, distinct from
  completed turn snapshots.
- Added a separate five-scenario capability suite covering long-horizon work, compiler recovery,
  workspace migration, stale observations, and dirty-tree user-work preservation.
- Added bounded compiler/test diagnostics with path, location, code, symbol, and recovery category
  metadata, plus capability metrics for success@1, recovery, stale evidence, unresolved work, and
  model-visible resource use.

#### Changed

- Restored contexts are revalidated against the live workspace before resumed actions and before a
  completion claim; stale evidence reopens the relevant work instead of being treated as progress.
- Incomplete interactive turns now persist their latest minimized context and continuation so
  `aether resume` can continue the active work without hidden conversational state.

#### Security

- Persisted work objectives are bounded and redact obvious secret assignments; checkpoints retain
  no raw prompt, assistant transcript, excerpt body, or command output.
- Completion remains blocked by unresolved path work, typed failures, stale evidence, user-owned
  changes, or unpassed verification.

### 2026-08-26 — Trajectory-guided interaction economy


#### Added

- Added bounded per-task trajectory analysis for repeated observations, search-to-read chains,
  verification recovery, and mutation-before-inspection candidates.
- Added lossless model-visible byte categories for static prefixes, tool schemas, context payloads,
  tool results, policy feedback, and protocol envelopes, plus provider/local timing metrics.

#### Changed

- Deterministic eval replay now reuses only exact read-only observations whose workspace state has
  not been invalidated by a mutation or verification step.
- Eval reports now retain baseline and optimized trajectories, reuse counts, and aggregate pattern
  counts so interaction changes can be compared without provider calls.

### 2026-08-26 — Leaner runtime measurement path


#### Changed

- Tool-schema byte accounting is now serialized lazily only when structured metrics are requested;
  normal agent construction no longer performs metric-only schema serialization.
- Removed the private scheduler compatibility wrapper so tests exercise the prepared-action
  scheduler path directly.
- Repaired the scheduler benchmark fixture to start from valid completed read-only workflow state,
  keeping orchestration measurements meaningful without changing production behavior.

### 2026-08-25 — Single-pass action execution and real eval gates


#### Added

- Added a real offline Agent end-to-end evaluation path with filesystem fixtures, deterministic
  model traces, before/after metrics, verification outcomes, and success@1 regression gates.
- Added a single prepared-action contract carrying normalized input, binary identity, typed
  effects, authority requirements, paths, provenance, and mutation/verification classification.
- Added packed Git inventory storage for 100k-file repositories and a shared bounded
  `ProcessRuntime` policy for finite commands and persistent stream reads.

#### Changed

- Reused prepared typed inputs across policy, guardrails, scheduling, context observation, and
  execution; planner observations now expose compact model text while retaining structured data.
- Scheduler selection now continues past blocked conflicts so independent ready calls can run.
- Runtime eval accounting now reports model requests, requested/executed/prevented calls, bytes,
  process spawns, verification attempts, wall/CPU time, RSS, allocation probes, and token usage.

#### Security

- Mutation admission remains bound to current inspected evidence and expected content hashes;
  action provenance is model-scoped and cannot convert tool output into user authorization.

### 2026-08-25 — Evidence-gated adaptive coding loop


#### Added

- Added evidence-gated completion feedback, compatible multi-read planning, phase-aware delta
  context, dirty-worktree ownership tracking, scoped verification recovery, and bounded adversarial
  eval coverage.

#### Changed

- The loop now treats policy and workflow evidence as the completion control-plane, coalesces safe
  read-only discovery, and requires current expected hashes for replacing inspected files.

#### Security

- Stale, user-owned, repeated-failure, and no-progress actions now fail closed or trip bounded
  circuit breakers without weakening workspace containment or permission gates.

### 2026-08-24 — Smaller production startup artifact


#### Changed

- Optimized release artifacts for size while retaining fat LTO, one codegen unit, aborting panic,
  stripped symbols, and portable CPU targeting.
- Added a Linux-compatible fresh-process startup probe for `--version`, `--help`, and minimal agent
  startup, kept separate from in-process Criterion measurements.

#### Performance

- Reduced stripped production binary size from 8,402,256 to 5,737,800 bytes (2,664,456 bytes,
  31.71%) while keeping agent decision paths speed-optimized and preserving agent, policy,
  planner, context, verification, tool, security, and deterministic evaluation behavior.

### 2026-08-24 — Runtime allocation and state sharing


#### Changed

- Shared immutable model tool definitions and lazily indexed symbol metadata instead of copying
  them for each model step or repository observation.
- Replaced full workflow/context clones, temporary path collections, and transient render strings
  with bounded projections, borrowed paths, and direct writes while preserving deterministic state.

#### Performance

- Release `aether` size is 8,402,264 bytes after the runtime changes; startup probes remained in
  the observed 0.05–0.06 second range on the local host.
- Deterministic evaluation remains 9/9 successful with 53 model steps, 38 executed tool calls,
  6 prevented redundant calls, and 5,874 context bytes.

### 2026-08-24 — Bounded repository action planning


#### Added

- Added a deterministic, read-only repository action planner that collapses high-confidence
  search, symbol/relationship lookup, and targeted reads into one bounded structured observation.
- Added strict ambiguity aborts, current-excerpt/range deduplication, containment and cancellation
  bounds, planner execution metrics, a planner benchmark, and a difficult multi-round-trip eval.

#### Performance

- Planner planning is pure and bounded; when current indexed context answers the request it emits
  no filesystem reads. Evaluation output now reports planner hit-rate, ambiguity aborts, planned
  actions, bytes, context, correctness, and before/after model/tool efficiency.

### 2026-08-23 — Direct command intelligence


#### Added

- Added bounded direct-argv command classification and effect extraction for Rust, Git, Node,
  Python, and common Unix inspection/search commands, including paths, packages, manifests, and
  verification scope.
- Added command-aware scheduler footprints, workflow invalidation, policy evidence gates,
  guardrail caching, and shell-heavy deterministic evaluation coverage.

#### Performance

- Added classification and footprint Criterion benchmarks; the local Criterion run measured
  direct-command classification at 116.70 ns median and footprint extraction at 1.9169 µs median.

### 2026-08-23 — Bounded autonomous coding policy


#### Added

- Added one deterministic policy layer that combines repository candidates, symbol and relationship
  evidence, context reuse, mutation evidence gates, focused verification ranking, scheduler-safe
  tool preflight, and no-progress escalation.
- Added bounded persisted decision state for evidence, unresolved questions, candidate files,
  modified scope, confidence, and the next actionable operation.
- Added baseline-versus-policy evaluation metrics, incomplete-evidence and repeated-exploration
  scenarios, plus policy update and next-action Criterion benchmarks.

#### Performance

- Policy observations reuse cached context and retain fixed-size state; the final local benchmark
  median was 10.273 µs per completed observation and 241.28 ns per next-action ranking, below
  the 20 µs overhead target.

### 2026-08-23 — Cross-file symbol relationship ranking


#### Added

- Added bounded, lazy lexical relationships for Rust definitions, callers, implementations,
  imports, dependencies, modules, and source/test files with ambiguity flags and hash invalidation.
- Added relationship-aware RepoMap and context selection ranking plus 10,000-symbol benchmarks.

#### Performance

- Relationship lookup and one-file update remain targeted to indexed participants; measured medians
  were 2.095 µs and 8.108 µs respectively on the local benchmark host.

### 2026-08-23 — Lightweight symbol-aware repository navigation


#### Added

- Added bounded, lazy Rust symbol extraction for functions, methods, types, modules, tests, and
  imports with targeted file lookup and in-memory content-hash invalidation.
- Added symbol-aware repository selection and context ranking plus parsing and 1,000-symbol lookup
  benchmarks.

#### Known Limitations

- Navigation is lexical and intentionally partial: it does not resolve macros, traits, generated
  code, cross-file references, or language semantics; TypeScript and Python are recognized for
  future parser extensions but are not yet extracted.

### 2026-08-23 — Structured execution metrics


#### Added

- Added opt-in JSON turn output with model, token, tool-call, context, read, verification, provider
  wait, and local execution metrics while preserving the default interactive renderer.
- Added accounting coverage for retries, cached usage events, missing provider usage, prevented
  calls, verification, cancellation, and metrics-disabled execution.

#### Performance

- Added a Criterion case for the metrics-disabled agent path.

### 2026-08-23 — Deterministic coding-agent evaluations


#### Added

- Added five isolated Rust coding fixtures covering targeted repair, repository navigation,
  multi-file edits, failed-first recovery, redundant-read avoidance, and focused verification.
- Added an offline deterministic evaluation runner with replaceable model integration, bounded
  step and tool budgets, clean workspace copies, machine-readable JSON, and compact summaries.

#### Performance

- CI regression gates now cover evaluation correctness, model steps, executed tool calls, and
  context bytes while reporting wall time and harness overhead without unstable timing gates.

### 2026-08-23 — Deterministic loop guardrails


#### Added

- Agent turns reuse bounded successful read, search, find, list, and verification observations
  within the same workspace revision and emit compact feedback for redundant or no-progress work.
- Successful required verification with no unresolved failures emits a deterministic completion
  signal, while failures, changed arguments, and workspace revisions remain retryable.

#### Performance

- Added fingerprint lookup and progress-state update benchmarks for the loop guardrail hot path.

### 2026-08-23 — Focused verification planning


#### Added

- Modified Rust, Node.js, and Python packages now receive deterministic focused verification
  plans, ordered from targeted tests through package-scoped checks with a safe unsupported-repo
  fallback.
- Bounded workflow state tracks required commands, affected scope, workspace revision, outcomes,
  and stale results; later mutations invalidate earlier verification.

#### Changed

- Direct process commands are conservatively classified as verification, mutation, or unknown,
  preventing ambiguous shell commands from satisfying workflow completion.

#### Performance

- Added benchmarks for verification planning and direct command classification.

### 2026-08-23 — Repository-aware coding workflow


#### Added

- Coding sessions now track bounded Discover, Inspect, Modify, Verify, and Complete phases,
  relevant files, mutation progress, verification status, and unresolved failures.
- Workspace mutations require current relevant inspection and successful mutations move the
  session into focused verification before completion.

#### Changed

- Workflow state is persisted with session context, remains recoverable across resumed sessions,
  and is rendered to the model as compact actionable status.

#### Performance

- Added benchmarks for bounded workflow updates and workflow context rendering overhead.

### 2026-08-23 — Relevance-ranked model context


#### Changed

- Stored recovery context is now selected separately for each model turn using deterministic
  task, path, symbol, recency, modification, source/test, and freshness signals.
- Model context is bounded by bytes and item count, deduplicates overlapping excerpts and
  repeated observations, and continues to invalidate stale file content on workspace drift.

#### Performance

- Context-selection benchmarks cover 100, 1,000, and 10,000 candidates with bounded hot-path
  allocations.

### 2026-08-23 — Lazy repository intelligence


#### Added

- Coding-task context now has a lazy, bounded `RepoMap` for tracked workspace files,
  manifests, package structure, source roots, tests, documentation, and scoped repository
  instructions.
- Repository-map invalidation tracks relevant structure and bounded metadata changes while
  ignoring source-content edits and ignored or untracked files; cold and cached 1k/10k-file
  benchmarks cover the behavior.

#### Performance

- Common Git index formats are discovered in-process, with fallback for indirect or unsupported
  indexes; benchmarks isolate repository-map work from external `git` process startup.

### 2026-08-23 — Low-overhead tool scheduling


#### Performance

- Ready-call selection uses caller-owned fixed buffers and active tool futures are
  polled directly, removing per-batch scheduler vectors, task spawning, and joins.
- Scheduler benchmarks now distinguish pure selection from end-to-end agent execution
  and the 2 ms Tokio-backed fixture.

### 2026-08-23 — Dependency-aware tool execution


#### Added

- Independent read-only tool calls now execute concurrently through a bounded scheduler,
  while workspace mutations, process effects, and conflicting footprints remain ordered.
- Typed tool-effect metadata, deterministic result ordering, cancellation coverage, and
  scheduler benchmarks were added.

### 2026-08-22 — State-root and metadata initialization hardening


#### Security

- State-root overrides now require an absolute, physically resolved path outside the canonical
  workspace before any AETHER state is created; traversal components and workspace-directed
  intermediate links are rejected.
- Workspace metadata is staged as a private temporary file and atomically installed without
  replacement, so concurrent initialization validates one binding and incomplete temps cannot
  become `workspace.json`.

### 2026-08-22 — Private application state and workspace isolation


#### Changed

- Persistent AETHER Fx application and session state now lives in OS application-state
  storage, never in a user workspace.
- Sessions are grouped under a deterministic BLAKE3 bucket for the canonical workspace,
  with bounded header-plus-tail discovery and workspace-scoped latest resume.
- `aether doctor` reports the canonical workspace, resolved state root, and resolved
  workspace session directory without exposing secrets.

#### Security

- `AETHER_FX_STATE_DIR` is an explicit state-root override; default resolution follows
  platform application-state conventions and never falls back to the current directory.
- `workspace.json` binds each bucket to its canonical workspace, while state directories,
  metadata, JSONL, and compaction temps retain private permissions and no-follow containment.
- Repository-local `.aether` and `.aether-fx` layouts are unsupported and ignored without
  migration, scanning, or fallback.

#### Documentation

- Documented the separation between repository data and private AETHER Fx application state,
  including platform-specific state-root resolution and the two independent boundaries.

### 2026-08-22 — CLI state isolation and session reliability


#### Changed

- Persistent sessions use private OS application-state storage, while repository-local
  AETHER state is ignored completely.
- Bounded session summaries discard partial UTF-8 boundary records before decoding,
  preserving discovery of the latest committed turn across tail and crash boundaries.

#### Security

- Session containment and no-follow protections cover the private state root, including
  workspace metadata, session JSONL files, and compaction temporaries.

### 2026-08-22 — Contained session discovery and reliable resume


#### Changed

- Session listing reads bounded header and tail metadata instead of replaying complete JSONL
  history, while preserving newest-first ordering.
- `aether resume --latest` skips corrupt, empty, unsupported, or otherwise unrestorable sessions
  until it finds newest valid candidate.

#### Security

- Session enumeration opens the selected workspace bucket, `sessions`, and JSONL entries
  through contained no-follow filesystem operations, rejecting links and reparse points.

### 2026-08-22 — CLI and session workflow improvements


#### Added

- Added `aether sessions` and safe `aether resume --latest` discovery within the selected workspace.

#### Changed

- One-shot and interactive prompts now use the same session-backed agent runtime path.

#### Security

- Session replay accepts only already-contained descriptors; missing sessions are handled with typed errors.

All notable AETHER Fx changes are recorded here, organized by public capability
within this release.

### 2026-08-22 — Session storage path hardening


#### Security

- State directories and JSONL files are created and opened without following symbolic
  links or Windows reparse points, and must remain inside the private state root.
- Compaction temps are created exclusively beside the selected OS-state session JSONL
  and cannot redirect writes through a planted symlink.

#### Documentation

- Documented session-path containment, no-follow open/create semantics, and
  residual TOCTOU between validation and the final OS call.

### 2026-08-22 — Session and context safety hardening


#### Changed

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

#### Security

- Unix session directories are mode `0700` and session JSONL/temp files are
  `0600`. `payload_contains_secrets` remains defense-in-depth after record
  minimization and is not complete secret detection.
- Compaction refuses to rewrite a session after corrupt replay, leaving the
  original file untouched.

#### Documentation

- Documented persisted versus omitted session fields, Unix session modes, turn
  commit/recovery, continuation invalidation, and the Rainy SDK 0.6.16 mapping
  limitation for continuation errors.

### 2026-08-22 — Context engine and session continuity


#### Added

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

#### Changed

- New-file `create_only` installation prefers native exclusive rename
  (Unix `renameat_with(NOREPLACE)`, Windows `MoveFileExW` without replace) and
  falls back to hard links only when exclusive rename is unavailable.

#### Security

- Session persistence refuses secret-bearing records and stores only opaque
  continuation identity keys. `create_only` still refuses to replace an
  existing destination.

#### Performance

- Re-ran the existing Criterion suite. Small-file read and session replay
  are slower in this configuration because `read` now hashes content and
  session JSONL validates schema v2; remaining statistically flagged
  changes are recorded in `docs/performance.md`.

#### Documentation

- Documented context/session bounds, resume behavior, and remaining
  filesystems that lack both exclusive rename and hard links.

#### Known limitations and constraints

- Filesystems without exclusive no-replace rename or hard links cannot
  atomically create-if-absent; AETHER fails closed rather than copying over an
  existing destination.
- Git awareness remains read-only; working-tree mutation is still out of
  scope.

### 2026-08-22 — Runtime correctness and concurrency hardening


#### Fixed

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

#### Changed

- Updated the six-target CI matrix to execute native workspace tests in
  addition to centralized quality and policy gates.
- Made process drain-task ownership and bounded five-second termination waits
  explicit in the runtime behavior.

#### Security

- Strengthened mutation precondition handling and process-lifecycle failure
  reporting without claiming cross-process compare-and-swap guarantees.

#### Performance

- Re-ran the existing benchmark suite and recorded current local measurements
  and observed regression signals in `docs/performance.md`.

#### Documentation updates

- Documented atomic replacement versus atomic create-if-absent, optimistic
  content preconditions, process cleanup aggregation, permission cancellation,
  native CI execution, and Rainy terminal-stream handling.

#### Known Limitations

- An arbitrary external process can still mutate the filesystem namespace in
  the final window after AETHER revalidation and before the atomic OS call;
  same-path AETHER races are serialized and the limitation is documented.

### 2026-08-22 — Native runtime hardening and interactive agent


#### Added

##### Cooperative runtime control

- Added the std-only `CancellationFlag` in `aether-core`, with agent-level
  notification support for interruptible async waits.
- Added `ToolExecutionContext` so every tool receives cancellation state and
  an authorization permit.
- Added cancellation checkpoints to filesystem reads, directory traversal,
  search matching, writes, patches, shell execution, persistent process I/O,
  and model streaming.
- Added a deterministic current-thread regression test proving blocking tool
  work does not monopolize the Tokio control plane.

##### Safe filesystem replacement

- Added a platform-aware atomic write path using same-directory temporary
  files, collision-resistant PID/counter names, exclusive creation, flush,
  file synchronization, cleanup, and precondition checks.
- Added the Windows `ReplaceFileW` implementation through `windows-sys 0.61.2`
  without deleting the destination before replacement.
- Added Unix executable-permission preservation tests and native replacement
  coverage for existing and new files.
- Added staged, rollback-capable multi-file patch commits with explicit
  high-severity errors when rollback is incomplete.

##### Persistent process runtime

- Added a bounded process registry with a maximum of 16 session-owned
  persistent processes.
- Added independent per-process stdin, stdout, stderr, and child-state
  synchronization so process I/O does not hold a global registry lock.
- Added continuous stdout/stderr drain tasks backed by 256 KiB per-stream
  bounded buffers, recent-output retention, and `dropped_bytes` metadata.
- Added session shutdown cleanup and cancellation-aware process start, read,
  write, signal, kill, and status behavior.

##### Interactive permissions

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

##### Multi-turn interactive session

- Changed bare `aether` into a persistent prompt loop that reuses the agent,
  backend, tool registry, process registry, permission grants, and session
  identity.
- Added stable session IDs and unique monotonic turn IDs.
- Added opaque Rainy continuation handoff between user turns.
- Added Ctrl+C turn cancellation that returns control to the interactive
  prompt while preserving established session processes.
- Preserved local prompt display before backend construction or Rainy network
  activity.

#### Changed

##### Tool execution and bounds

- Moved potentially blocking filesystem, canonicalization, hashing, traversal,
  and search work to bounded `spawn_blocking` operations while retaining the
  current-thread Tokio scheduler.
- Kept shell execution as direct argv spawning and Git operations read-only.
- Kept ordinary tool failures and permission denials as structured tool
  results for model recovery instead of terminating the whole turn.
- Kept unknown tool calls fail-closed with `unknown_tool`.
- Retained exactly nine model-visible tools: `read`, `list`, `find`, `search`,
  `write`, `patch`, `shell`, `process`, and `git`.

##### Deterministic Rainy selection

- Changed model selection to `--model`, then `AETHER_MODEL`, then a clear
  configuration error.
- Removed catalog-order fallback and unnecessary catalog fetches from normal
  model-selected inference.
- Kept `rainy-sdk 0.6.16`; no unpublished Git dependency or speculative SDK
  upgrade was introduced.

#### Tests

- Added coverage for atomic replacement, temporary cleanup, precondition
  mismatch, Unix executable permissions, staged patch rollback, typed
  cancellation, traversal cancellation, model-stream cancellation, process
  concurrency, process draining, bounded output, process limits, permission
  decisions, permit binding, continuation identity, stable sessions, model
  precedence, and no-network startup.
- Added large shell output, large persistent-process output, and large
  workspace bounds coverage.
- Added a registry assertion that exactly nine model-visible tools exist.

#### Documentation

- Updated architecture documentation for the blocking worker boundary,
  process subsystem, cancellation, permission broker, and nine-tool executor.
- Updated security documentation for atomic replacement, workspace
  containment, permits, session grants, process ownership, cleanup, bounded
  output, cancellation, and non-interactive fail-closed behavior.
- Updated tool contracts with process buffer and truncation metadata.
- Updated Rainy integration, toolchain baseline, and performance documentation.

#### Dependencies

- Enabled the existing `windows-sys 0.61.2` `Win32_Storage_FileSystem`
  feature for `ReplaceFileW`.
- Added direct `serde_json` use to the terminal permission renderer and the
  Windows-targeted `windows-sys` edge to `aether-tools`.
- Added no new crate package and kept the existing Rust `1.98.0` toolchain.

#### Performance

- Added benchmarks for blocking dispatch, cancellation checks, permission
  decisions, multi-turn bookkeeping, atomic writes, staged patches, process
  buffer reads, and process registry lookup.
- Recorded the current release binary size as 4,943,824 bytes (4.71 MiB).
- Recorded 225 resolved lockfile packages; no new package dependency was
  introduced.
- No unsupported before/after speedup claim is made where no pre-Phase 1
  baseline was available.

#### Known Limitations

- Native Windows execution and the complete six-target CI matrix remain
  pending external CI verification.
- Full `aether resume` remains unsupported; in-process multi-turn continuity
  is the supported Phase 1 behavior.
- Extended attributes, ACLs, and alternate data streams are not claimed as
  preserved by atomic replacement.
- PTY support, Git mutations, shell interpreter semantics, and live Rainy
  probing remain out of scope for this phase.

### 2026-08-21 — Foundational bootstrap and runtime hardening


#### Added

##### Workspace and repository foundation

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

##### Runtime boundaries

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

##### Exact v0.1 tool surface

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

##### Authorization and terminal interaction

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

##### CLI and Rainy behavior

- Added `aether --version`, `--help`, `doctor`, `models`, prompt mode, and a
  minimal interactive shell.
- Added explicit model selection through `--model` or `AETHER_MODEL`; model
  selection fails closed instead of silently choosing a catalog position.
- Added multi-turn interactive continuation using opaque Rainy response IDs.
- Preserved the startup rule: the bare `aether` prompt appears without an
  immediate Rainy network request.
- Added explicit unsupported responses for capabilities that are not yet
  implemented rather than placeholder behavior.

##### Tests, benchmarks, and documentation

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

#### Changed

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

#### Security

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

#### Performance

- Kept tool output, search, process buffers, agent text, event queues, and
  session replay bounded.
- Kept public release builds free of `target-cpu=native` and configured fat
  LTO, one codegen unit, stripped symbols, and aborting panic.
- Recorded the current exploratory search benchmark in
  `docs/performance.md`; it is a measurement baseline, not a product claim.

#### Known Limitations

- The verified Rainy SDK is `0.6.16`; the anticipated `6.5.x` generation was
  not available and has not been fabricated.
- Live Rainy integration tests remain opt-in and require `RAINY_API_KEY`.
- `aether resume` remains explicitly unsupported in this bootstrap phase.
- Full native validation for every release target belongs to CI; local
  cross-compilation is host-toolchain dependent.
- Release tooling is prepared in CI but no SBOM, attestation, or release
  artifact has been published by this work.
