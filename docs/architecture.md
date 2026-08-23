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
  ├── short registry lookup lock
  ├── independent stdin/stdout/stderr state
  ├── bounded continuously-drained output buffers
  └── confirmed termination with bounded shutdown waits
Rainy adapter
  └── verified terminal Responses events and opaque continuation data
```

`aether-core` owns IDs, bounded values, typed errors, paths, permissions, events, context snapshot types, and model/tool request primitives. It has no terminal, Rainy, or platform dependency.

`aether-agent` owns the backend-neutral turn loop, cancellation, permission handshake, continuation, bounded context engine, and local JSONL session store. Its `ModelBackend` contract uses bounded Tokio channels and does not expose Rainy types. Interactive turns reuse the agent, tool registry, process registry, permission grants, backend, and working context; cancellation returns control to the next prompt without destroying established session processes. A permission prompt reports terminal cancellation separately from an ordinary denial, so Ctrl+C cancels the whole turn and cannot become a model-visible denial. Session append/replay/compact run in `spawn_blocking` so the current-thread control task does not perform session I/O.

`aether-rainy` is the only crate that imports `rainy_sdk`. It maps the SDK's dynamic Responses stream values into AETHER `ModelEvent` values and treats provider reasoning metadata as opaque continuation data. A successful `ModelEvent::Done` requires a verified `response.completed` event and a response identity; failed, incomplete, transport-error, and unexpected-EOF paths remain backend errors. No provider endpoint, model catalog, or key appears elsewhere.

`aether-tools` owns exactly nine model-visible tools and implements workspace containment, output bounds, permit validation, filesystem, process, search, patch, and read-oriented Git behavior. Filesystem-heavy work runs in one bounded blocking operation per tool call. `write` and `patch` share a weak-reference per-destination coordinator; multi-file patches acquire normalized keys in sorted order, then stage and revalidate before sequential commit. Persistent process registry locks cover only lookup/insert/remove/count; no process I/O is awaited while holding them. Process handles retain drain-task ownership and remain registered when kill or wait confirmation fails.

`aether-terminal` owns raw input, restoration, ANSI/VT presentation, incremental buffer rendering, and platform modules. Tools and agent code never write ANSI.

There is no RPC, daemon, database, plugin system, WebView, TUI framework, or alternate allocator in this bootstrap.

Local sessions are versioned JSONL files under `<workspace>/.aether/sessions/<session-id>.jsonl`. Schema version 3 stores `Started`, committed `TurnSnapshot` records, and `Finished`. Each completed turn is one JSONL record containing turn metadata, minimized context (paths/hashes/ranges and safe tool summaries), and sanitized continuation. Raw prompts, assistant text, excerpt bodies, and command output are not persisted. `aether resume <session-id>` restores the last committed turn, re-hashes inspected files, and discards Rainy continuation when the workspace root or inspected files have changed. Unsupported older schemas fail closed. Truncated trailing records are ignored; corrupt replay never compact-overwrites the original file.

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
