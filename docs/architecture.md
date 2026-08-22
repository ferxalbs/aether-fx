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

The live interactive shape is:

```text
terminal
  ↕ prompt, rendering, permission decisions
agent runtime
  ├── cancellation token + std-only cancellation flag
  ├── permission broker + session grants
  ├── stable SessionId / TurnId and opaque continuation
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
  ├── short registry lookup lock
  ├── independent stdin/stdout/stderr state
  ├── bounded continuously-drained output buffers
  └── confirmed termination with bounded shutdown waits
Rainy adapter
  └── verified terminal Responses events and opaque continuation data
```

`aether-core` owns IDs, bounded values, typed errors, paths, permissions, events, and model/tool request primitives. It has no terminal, Rainy, or platform dependency.

`aether-agent` owns the backend-neutral turn loop, cancellation, permission handshake, continuation, and session state. Its `ModelBackend` contract uses bounded Tokio channels and does not expose Rainy types. Interactive turns reuse the agent, tool registry, process registry, permission grants, and backend; cancellation returns control to the next prompt without destroying established session processes. A permission prompt reports terminal cancellation separately from an ordinary denial, so Ctrl+C cancels the whole turn and cannot become a model-visible denial.

`aether-rainy` is the only crate that imports `rainy_sdk`. It maps the SDK's dynamic Responses stream values into AETHER `ModelEvent` values and treats provider reasoning metadata as opaque continuation data. A successful `ModelEvent::Done` requires a verified `response.completed` event and a response identity; failed, incomplete, transport-error, and unexpected-EOF paths remain backend errors. No provider endpoint, model catalog, or key appears elsewhere.

`aether-tools` owns exactly nine model-visible tools and implements workspace containment, output bounds, permit validation, filesystem, process, search, patch, and read-oriented Git behavior. Filesystem-heavy work runs in one bounded blocking operation per tool call. `write` and `patch` share a weak-reference per-destination coordinator; multi-file patches acquire normalized keys in sorted order, then stage and revalidate before sequential commit. Persistent process registry locks cover only lookup/insert/remove/count; no process I/O is awaited while holding them. Process handles retain drain-task ownership and remain registered when kill or wait confirmation fails.

`aether-terminal` owns raw input, restoration, ANSI/VT presentation, incremental buffer rendering, and platform modules. Tools and agent code never write ANSI.

There is no RPC, daemon, database, plugin system, WebView, TUI framework, or alternate allocator in this bootstrap.

The existing bounded JSONL `SessionStore` remains an opt-in persistence primitive; Phase 1 interactive continuity is in-process and does not call its synchronous replay/append methods from the Tokio control task. Full resume is intentionally unsupported.
