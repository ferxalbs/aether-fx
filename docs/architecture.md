# Architecture

AETHER Fx remains one binary and one process on the inactive path. The optional browser path adds
one separately supervised Obscura process only after `/browser` consent. Tokio stays on the
current-thread scheduler; potentially slow filesystem and CPU work is isolated in bounded
`spawn_blocking` operations.

```text
aether
  ├── aether-agent
  │     └── aether-core
  ├── aether-rainy
  │     ├── aether-agent (implements ModelBackend)
  │     └── rainy-sdk
  ├── aether-obscura
  │     └── aether-core
  ├── aether-tools
  │     ├── aether-core
  │     └── aether-obscura
  ├── aether-tui
  │     └── aether-core
  └── aether-terminal
        └── aether-core
```

The runtime path is:

```text
terminal input → aether-tui → aether-agent → ModelBackend → aether-rainy → Rainy SDK → Rainy API
                         └────── ToolExecutor → aether-tools
                                                   └────── optional stdio MCP → Obscura → browser engine
agent events ───────────────────────────────────────→ aether-tui → ratatui frame
legacy/headless input and output ───────────────────→ aether-terminal
```

## Interactive boundary and model presentation

The binary has one stateful TUI shell path and one headless/legacy path. The TUI is enabled only
when both standard streams are TTYs and the invocation is not JSON, one-shot, or explicitly
`--non-interactive`. `aether-tui` owns the state machine and ratatui presentation: its header,
structured transcript, multiline composer, footer, slash palette, model/session pickers,
transcript navigation, schedule form/review, and approval overlays emit typed `TuiAction` values.
The binary remains responsible for constructing agent requests, selecting the existing permission
broker, starting/cancelling turns, loading catalogs and sessions, calling the local calendar
adapter, and persisting bounded state. Pipe, JSON, and one-shot paths use plain newline-delimited
input/output and retain the existing permission and renderer behavior; they never inherit selector
or raw-mode behavior.

`TuiSession` is the TUI's RAII terminal owner. It consumes crossterm events, enters raw mode and
bracketed paste, draws the inline transcript viewport, temporarily uses the alternate screen for
pickers and overlays, and restores cursor, wrapping, paste, screen, and raw-mode state on normal
return, cancellation, error, or unwind. Overlay closure returns to the inline shell without
creating a second agent or persistence path. The interactive event pump multiplexes TUI events,
agent events, cancellation, and turn completion through bounded channels; queued prompts are
memory-only and capped at eight.

Leading slash input is recognized only at the beginning of a TUI composer. The palette is local
and does not become model input. `/status`, `/context`, `/config`, `/doctor`, `/help`, `/clear`,
`/exit`, and `/transcript` remain local operations; `/model` and `/sessions` load the existing
catalog/session boundaries. `/schedule` opens a TUI-only form and review card, then emits a typed
draft for the binary's local calendar adapter only after explicit confirmation.

`aether-rainy` owns one normalized `ModelView` projection of each SDK catalog entry. It preserves
unknowns instead of guessing from model names, records the SDK's model context,
tools/reasoning/modalities, and supported parameters, and retains compatibility fields as unknown
when the SDK catalog does not provide them. `/model`, `aether models`, `/status`, and the JSON model
surface consume this projection. A selected model is applied to future turns, while its Rainy
continuation and seeded context continuation are cleared before the next request so a provider response from one model is
never sent to another.

The autonomous work loop is deliberately evidence-driven rather than a second planner:

```text
understand → acquire bounded repository evidence → decide → act → observe
     ↑                                                        ↓
     └──── recover from typed failure / stale evidence ← verify
                              ↓
                       update WorkState → continue or finish
```

Every model tool event becomes one `PreparedAction` at the registry boundary. It carries the
normalized typed input, stable binary fingerprint, bounded resource footprint, action class,
permission/authority requirements, normalized paths, command effects, and model provenance. Policy,
guardrails, scheduling, mutation admission, context observation, and execution consume that same
record; typed registry dispatch reuses its parsed input instead of deserializing it again. Invalid
inputs remain conservative and use the compatibility path.

The live interactive shape is:

```text
terminal
  ↕ prompt, rendering, permission decisions
agent runtime
  ├── cancellation token + std-only cancellation flag
  ├── permission broker + session grants
  ├── stable SessionId / TurnId and opaque continuation
  ├── bounded context engine + JSONL SessionStore
  └── one RainyBackend instance per session
local calendar boundary
  ├── provider-neutral CalendarAdapter
  └── LocalCalendarAdapter → private OS state `workspaces/<workspace-id>/appointments.json`
tool executor
  ├── execution permit bound to call_id + tool + permission class
  ├── cooperative cancellation
  └── exactly nine tools
blocking filesystem pool
  ├── read/list/find/search
  ├── write/patch preparation and staged replacement
  └── weak per-destination mutation locks
process subsystem
  ├── shared ProcessRuntime output/deadline bounds for finite and persistent modes
  ├── short registry lookup lock
  ├── independent stdin/stdout/stderr state
  ├── bounded continuously-drained output buffers
  └── confirmed termination with bounded shutdown waits
Rainy adapter
  └── verified terminal Responses events and opaque continuation data
optional Obscura provider
  ├── fixed release manifest and verified private installation
  ├── one process + one serialized newline-delimited MCP channel per session
  ├── validated six-tool allowlist over `BrowserSession(id)`
  └── bounded shutdown and status-only durable summaries
```

`aether-core` owns IDs, bounded values, typed errors, paths, permissions, events, context snapshot types, and model/tool request primitives. It has no terminal, Rainy, or platform dependency.

`aether-agent` owns the backend-neutral turn loop, cancellation, permission handshake, continuation, bounded context engine, and local JSONL session store. Its `ModelBackend` contract uses bounded Tokio channels and does not expose Rainy types. Interactive turns reuse the agent, tool registry, process registry, permission grants, backend, and working context; cancellation returns control to the next prompt without destroying established session processes. A permission prompt reports terminal cancellation separately from an ordinary denial, so Ctrl+C cancels the whole turn and cannot become a model-visible denial. Session append/replay/compact run in `spawn_blocking` so the current-thread control task does not perform session I/O.

For complex prompts, `aether-core::WorkState` adds a compact adaptive work ledger: normalized objective, explicit acceptance criteria, bounded path subgoals, dependencies, active status, evidence, mutations, verification records, and typed blockers. A model may optionally send one bounded `<aether-work>` JSON outline containing subgoal intent, dependencies, and paths; the runtime validates that outline but ignores model-declared completion, blocker, criterion, evidence, mutation, and verification fields. Simple prompts retain the simple path. The runtime records observable work and refuses completion when concrete structured work, unsatisfied criteria, or blockers remain. Failed verification output is reduced to bounded path/line/column/code/symbol diagnostics, and the runtime promotes a canonical, bounded source window for those paths. Successful mutations similarly refresh affected source evidence; exact subsequent reads may reuse it only after a live hash check. Restored contexts are re-hashed against the live workspace before any resumed action and again before completion.

Observation freshness is deliberately asymmetric. Successful non-read discovery and verification
observations are not served from the generic cache unless all of their dependencies have a proven
current revision; when that proof is unavailable, the tool executes again. This conservative extra
call preserves current repository navigation and verification evidence at the cost of some local
latency. Exact reads retain the cheaper reuse path only after a bounded live hash of the canonical
file matches the retained hash. A known, certain shell mutation may clear stale workspace state
only after every affected path has been refreshed successfully; uncertain commands or partial
refreshes leave the state stale and route the loop through recovery. Installing a verification
plan resets its steps to `pending`, and completion accepts only an observed successful verification
for the current workspace revision. The deterministic eval suite includes a negative premature-
completion trajectory to ensure a finish claim without required work or observed verification is
rejected.

Context selection is scoped to the active workflow: current objective and subgoal, modified and
user-owned paths, current diagnostics, inspected excerpts, symbol/relationship hits, and relevant
tests are ranked before broad repository metadata. Effective `AGENTS.md`/`CLAUDE.md`-style
instructions are merged broad-to-specific only for active paths; unrelated instruction bodies are
not promoted into the model packet. Recovery packets are intentionally smaller than full context
packets and contain only affected paths plus the newest bounded source windows.

An interrupted or step-limited turn is appended as a `Checkpoint`, distinct from a completed `TurnSnapshot`. Checkpoints contain only the same minimized context and sanitized continuation allowed for normal session persistence. Resume therefore recovers the latest active work state, unresolved blockers, current workspace evidence, and completion status without relying on hidden conversation history. The runtime remains single-agent; the repository action planner is a bounded read-only optimization for high-confidence local evidence, not an additional model or project-management system.

`aether-rainy` is the only crate that imports `rainy_sdk`. It resolves each selected model to the
OpenAI Responses or OpenAI-compatible Chat Completions protocol, translates the bounded neutral
conversation and tool schemas, and maps both typed streams into AETHER `ModelEvent` values;
provider reasoning metadata remains outside the agent contract. Responses requires a verified
`response.completed` event and response identity; Chat Completions requires a terminal finish
chunk. Failed, incomplete, transport-error, and unexpected-EOF paths remain backend errors. No
provider endpoint, model catalog, or key appears elsewhere.

`aether-tools` owns exactly nine model-visible tools and implements workspace containment, output bounds, permit validation, filesystem, process, search, patch, and read-oriented Git behavior. When a validated Obscura supervisor is active, the registry is rebuilt at a turn boundary with exactly six additional fixed browser definitions. Filesystem-heavy work runs in one bounded blocking operation per tool call. `write` and `patch` share a weak-reference per-destination coordinator; multi-file patches acquire normalized keys in sorted order, then stage and revalidate before sequential commit. Persistent process registry locks cover only lookup/insert/remove/count; no process I/O is awaited while holding them. Finite commands and persistent stream reads share `ProcessRuntime` deadline/output ceilings; handles retain drain-task ownership and remain registered when kill or wait confirmation fails. The hosted WebMCP surface does not enter this registry.

`aether-obscura` is the only crate that knows the external browser wire contract. It owns the
static v0.2.1 artifact manifest, consent-gated installation, archive verification, absolute
process spawn, newline-delimited MCP framing, schema validation, bounded stderr/result handling,
fixed performance audit, storage-state persistence, and shutdown. Obscura's advertised catalog is
never copied into the AETHER model registry: provider operations map to six compiled definitions,
and the internal evaluator used by `browser.performance_audit` is not model-visible.

The optional runtime sequence is:

```text
/browser ──consent──> verify/install ──> spawn once ──> initialize/tools-list ──> active
    │                                                                                 │
    ├── /browser <task> → ExternalResearch (read-only)                               │
    ├── normal prompt  → Repository mode, with browser tools still available          │
    ├── /browser stop  → shutdown + retain private profile                            │
    └── /clear,/exit  → shutdown + close AETHER session                               │
```

The `ExternalResearch` mode changes completion and surface policy only for the turn started by
`/browser <task>`. It does not create a second agent or Rainy backend. Rebuilding the `Agent` for
provider activation, stop, or model selection keeps the context snapshot, continuation identity,
and the still-healthy supervisor separate from the agent value.

Repository inventory discovery packs all Git paths into one UTF-8 slab with 32-bit ranges. Only
bounded selected metadata is promoted to `PathBuf`s, so a 100k-file repository does not create
100k transient path allocations and the model-facing map remains capped by `RepoMapLimits`. The
Git index and fallback `git ls-files` stream are capped at 64 MiB, with stderr retained only up to
4 KiB, so malformed or unusually large repository metadata cannot create unbounded inventory
memory.

`aether-tui` owns interactive presentation and state transitions but never authorizes tools, starts
agent turns, persists sessions, or contacts a provider. It depends only on `aether-core` and the
terminal/UI stack. `aether-agent` owns the provider-neutral `CalendarAdapter` boundary and its
bounded local implementation; it performs no network work for `/schedule`. `aether-terminal`
continues to own raw input, restoration, ANSI/VT presentation, incremental buffer rendering, and
platform modules for legacy/headless compatibility. Tools and agent code never write ANSI.

There is no RPC, daemon, database, plugin system, or WebView in the native bootstrap. The focused
`aether-tui` crate is the only native presentation framework; it is not a provider, tool runtime,
or native bridge. Alpha-06 adds `aether-web` as a separately built `cdylib`/`rlib` for a static
browser demo; it is not linked into the `aether` binary and has no native bridge.

## Hosted WebMCP vertical slice

The browser surface is deliberately a different deployment boundary from the native graph:

```text
static HTML/CSS/JS
       ├── document.modelContext.registerTool ──> six page tool callbacks
       └── wasm-bindgen JS ──> aether-web::BrowserRuntime ──> aether-core::WorkflowState
                                                     └── bounded in-memory demo repository
```

The page's visible controls and WebMCP callbacks call the same runtime operation functions. Read
tools inspect the fixture; `propose_patch` stores an exact-match, hash-bound staged view; and
`verify_patch` runs fixed browser-safe contract checks. A visible human Accept or Reject operation
is not registered as a WebMCP tool. The browser path cannot read the checkout, spawn a process,
contact Rainy, use Obscura, or persist credentials. Unsupported WebMCP hosts receive a visible
compatibility notice while the page remains usable. Full implementation and the challenge demo
script are in [`docs/webmcp.md`](webmcp.md) and [`docs/webmcp-challenge.md`](webmcp-challenge.md).

The bounded Turn Director projects observed state into Discover, Understand, Implement, Recover,
Verify, Review, or Done; it never accepts a model phase claim and does not script the model's
strategy. A separate in-memory WorkingSet projects the objective, active subgoal, pending
criteria, changed/user-owned paths, diagnostics, definitions/relationships, mutations, freshness,
and blockers into a small deterministic view. Full excerpts remain in ContextSnapshot and are
selected only when needed. Typed failures feed a bounded Recovery Controller: stale evidence is
invalidated and refreshed, compiler/test and patch failures request a new decision, and
permission/environment failures remain blockers. The verification director orders targeted checks
before package or workspace-dependent checks; public Rust module changes in multi-package
workspaces require the broader dependent check. Completion is a current-revision review over task
state, evidence, verification, blockers, and user-owned paths, and fails closed when any required
fact is missing or stale. None of these freshness or completion controls bypasses the existing
permission, canonical-path, containment, permit, or expected-hash checks.

Repositories contain user/project data. AETHER Fx persistent application and session state lives outside repositories in private OS application-state storage. The resolver honors `AETHER_FX_STATE_DIR`; otherwise Linux/Unix uses `$XDG_STATE_HOME/aether-fx` or `$HOME/.local/state/aether-fx`, macOS uses `$HOME/Library/Application Support/aether-fx`, and Windows uses `%LOCALAPPDATA%\\aether-fx`. State is grouped as `workspaces/<BLAKE3(canonical-native-workspace-path)>/workspace.json`, `workspaces/<BLAKE3(canonical-native-workspace-path)>/sessions/<session-id>.jsonl`, and the local-only `appointments.json` in the same workspace bucket. `workspace.json` binds the bucket to the canonical workspace path, and session discovery is scoped to that bucket. Schema version 6 stores `Started`, committed `TurnSnapshot` records, resumable `Checkpoint` records, and `Finished`; schemas 3–5 remain readable. Each turn record contains only bounded metadata, minimized context (paths/hashes/ranges, structured work state, and safe tool summaries), sanitized continuation, and an optional bounded safe semantic transcript. Raw tool output, command arguments, process output, file contents, permission details, and credentials are not persisted. State directories and files are created and opened without following symlinks or Windows reparse points; session JSONL, appointment state, and compaction temps remain contained under the OS state root. `aether resume <session-id>` restores the latest committed turn or checkpoint, re-hashes inspected files, and discards Rainy continuation when the workspace root or inspected files have changed. Unsupported older schemas fail closed. Truncated trailing records are ignored; corrupt replay never compact-overwrites the original file.

The Obscura profile is a separate child of that same private state root, keyed by the canonical
workspace ID. It contains only adapter-managed provider storage state; it is not part of the
repository session JSONL and is not interpreted as AETHER context. Stopping or clearing the
session retains that profile for a later explicit activation, while process identity, active tabs,
and MCP request state are intentionally ephemeral. `/resume` never starts the provider merely
because a profile exists.

Context bounds (also documented on the constants in `aether-core::context`):

| Limit | Value |
| --- | ---: |
| Session file size | 2 MiB |
| Session JSONL line | 256 KiB |
| Session lines | 4,096 |
| Stored turns | 64 |
| Tool summaries | 32 |
| Context items / modified paths | 48 |
| Inspected files | 64 |
| File excerpts | 16 |
| Excerpt size | 4 KiB |
| Compact summary | 2 KiB |
| Git snapshot | 4 KiB |
| Context packet | 24 KiB |
| Work objective | 512 B |
| Work subgoals | 16 |
| Work evidence / mutations / verifications | 32 / 24 / 24 |
| Work blockers | 8 |
