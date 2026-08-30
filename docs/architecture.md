# Architecture

AETHER Fx remains one binary and one process. Tokio stays on the current-thread scheduler; potentially slow filesystem and CPU work is isolated in bounded `spawn_blocking` operations.

```text
aether
  ├── aether-agent
  │     └── aether-core
  ├── aether-rainy
  │     ├── aether-agent (implements ModelBackend)
  │     └── rainy-sdk
  ├── aether-tools
  │     └── aether-core
  └── aether-terminal
        └── aether-core
```

The runtime path is:

```text
terminal input → aether-agent → ModelBackend → aether-rainy → Rainy SDK → Rainy API
                         └────── ToolExecutor → aether-tools
agent events ───────────────────────────────────────→ aether-terminal
```

## Interactive boundary and model presentation

The binary has one stateful shell path and one headless path. The shell is enabled only when both
standard streams are TTYs and the invocation is not JSON, one-shot, or explicitly
`--non-interactive`. It prints a compact identity line without contacting Rainy, then reuses the
same bounded terminal substrate for the prompt, slash-command palette, model/session selectors,
confirmations, secret input, permission decisions, and incremental event rendering. Pipe, JSON,
and one-shot paths use plain newline-delimited input/output and a fail-closed permission broker;
they never inherit selector or raw-mode behavior.

Leading slash input is recognized only at the beginning of a shell line. The palette is local and
does not become model input. `/status`, `/context`, `/config`, `/doctor`, `/help`, `/clear`, and
`/exit` remain offline/local operations; `/model` is the explicit live catalog boundary.

`aether-rainy` owns one normalized `ModelView` projection of each SDK catalog entry. It preserves
unknowns instead of guessing from model names, distinguishes model, plan, and effective context,
records tools/reasoning/modalities and access state, and carries only bounded data-policy
disclosure fields. `/model`, `aether models`, `/status`, and the JSON model surface consume this
projection. A selected model is applied to future turns, while its Rainy continuation and seeded
context continuation are cleared before the next request so a provider response from one model is
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

`aether-rainy` is the only crate that imports `rainy_sdk`. It maps the SDK's dynamic Responses stream values into AETHER `ModelEvent` values and treats provider reasoning metadata as opaque continuation data. A successful `ModelEvent::Done` requires a verified `response.completed` event and a response identity; failed, incomplete, transport-error, and unexpected-EOF paths remain backend errors. No provider endpoint, model catalog, or key appears elsewhere.

`aether-tools` owns exactly nine model-visible tools and implements workspace containment, output bounds, permit validation, filesystem, process, search, patch, and read-oriented Git behavior. Filesystem-heavy work runs in one bounded blocking operation per tool call. `write` and `patch` share a weak-reference per-destination coordinator; multi-file patches acquire normalized keys in sorted order, then stage and revalidate before sequential commit. Persistent process registry locks cover only lookup/insert/remove/count; no process I/O is awaited while holding them. Finite commands and persistent stream reads share `ProcessRuntime` deadline/output ceilings; handles retain drain-task ownership and remain registered when kill or wait confirmation fails.

Repository inventory discovery packs all Git paths into one UTF-8 slab with 32-bit ranges. Only
bounded selected metadata is promoted to `PathBuf`s, so a 100k-file repository does not create
100k transient path allocations and the model-facing map remains capped by `RepoMapLimits`. The
Git index and fallback `git ls-files` stream are capped at 64 MiB, with stderr retained only up to
4 KiB, so malformed or unusually large repository metadata cannot create unbounded inventory
memory.

`aether-terminal` owns raw input, restoration, ANSI/VT presentation, incremental buffer rendering, and platform modules. Tools and agent code never write ANSI.

There is no RPC, daemon, database, plugin system, WebView, TUI framework, or alternate allocator in this bootstrap.

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

Repositories contain user/project data. AETHER Fx persistent application and session state lives outside repositories in private OS application-state storage. The resolver honors `AETHER_FX_STATE_DIR`; otherwise Linux/Unix uses `$XDG_STATE_HOME/aether-fx` or `$HOME/.local/state/aether-fx`, macOS uses `$HOME/Library/Application Support/aether-fx`, and Windows uses `%LOCALAPPDATA%\\aether-fx`. State is grouped as `workspaces/<BLAKE3(canonical-native-workspace-path)>/workspace.json` plus `workspaces/<BLAKE3(canonical-native-workspace-path)>/sessions/<session-id>.jsonl`. `workspace.json` binds the bucket to the canonical workspace path, and session discovery is scoped to that bucket. Schema version 5 stores `Started`, committed `TurnSnapshot` records, resumable `Checkpoint` records, and `Finished`. Each turn record contains only bounded metadata, minimized context (paths/hashes/ranges, structured work state, and safe tool summaries), and sanitized continuation. Raw prompts, assistant text, excerpt bodies, and command output are not persisted. State directories and files are created and opened without following symlinks or Windows reparse points; session JSONL and compaction temps remain contained under the OS state root. `aether resume <session-id>` restores the latest committed turn or checkpoint, re-hashes inspected files, and discards Rainy continuation when the workspace root or inspected files have changed. Unsupported older schemas fail closed. Truncated trailing records are ignored; corrupt replay never compact-overwrites the original file.

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
