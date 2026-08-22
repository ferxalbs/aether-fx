# Tool contracts

The model-visible registry has exactly nine names:

| Tool | Permission | v0.1 contract |
| --- | --- | --- |
| `read` | ReadOnly | One or more bounded workspace file reads with line ranges, UTF-8/binary detection, partial output, and line numbers |
| `list` | ReadOnly | Ignore-aware bounded directory entries with depth, hidden policy, and metadata |
| `find` | ReadOnly | Ignore-aware path/name globs such as `**/*.rs` |
| `search` | ReadOnly | Ignore-aware bounded literal/regex content search with globs, case options, context, and binary exclusion |
| `write` | WorkspaceWrite | Bounded UTF-8 replacement/create with optional BLAKE3 precondition, same-directory staging, per-destination AETHER serialization, and atomic replacement or commit-time no-replace create; existing basic permissions are preserved |
| `patch` | WorkspaceWrite | Multiple strict hunks/files, preconditions, deterministic multi-path locking, staged commit, immediate pre-commit revalidation, and rollback-capable best-effort transaction; not globally atomic |
| `shell` | ProcessExecute | Direct `program` plus `arguments`, `cwd`, exit status, duration, bounded stdout/stderr, and truncation metadata |
| `process` | ProcessPersistent | `start`, `read`, `write`, `signal`, `status`, and `kill` with stable process IDs; direct argv only, 16-process session cap, continuously-drained bounded stdout/stderr, bounded termination confirmation, and `buffered_bytes`/`dropped_bytes` metadata on reads/status |
| `git` | ReadOnly in v0.1 | Structured status, diff, show, log, and branch inspection through the installed Git executable |

Internal helpers are not registered as model-visible tools. Each request and result is typed and serializable; output is capped before it reaches model context. Permission, cancellation, model selection, session, and terminal facilities are runtime controls, not tools.

The interactive permission decisions are `allow_once`, `allow_session`, and `deny`. A denied tool call is returned to the model as a structured `permission_denied` result so the model can adapt. Ctrl+C and end-of-input are separate terminal outcomes that cancel the active turn rather than producing a denial; the blocking prompt worker finishes before control returns to the session. Unknown tool names fail closed as `unknown_tool`.
