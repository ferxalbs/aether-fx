# Security model

## Workspace containment

Filesystem tools accept a canonical workspace root. User paths are rejected when absolute, lexical traversal escapes the root, canonicalization escapes the root, or a symlink/junction resolves outside it. Every tool uses the shared workspace resolver.

## Permissions

The typed permission classes are `ReadOnly`, `WorkspaceWrite`, `ProcessExecute`, `ProcessPersistent`, `GitMutate`, `OutsideWorkspace`, and `NetworkMutation`. Read-only calls receive an automatic permit. Other calls produce a structured request and are resolved as allow once, allow for the current session, or deny. Session grants are keyed by tool name plus permission class; a shell grant cannot authorize `write` or `git`.

The agent passes an `ExecutionPermit` into every tool. The tool validates the exact `call_id`, tool name, and permission class before mutation or process execution, so a terminal prompt is not the only authorization boundary. Interactive approval is fail-closed when stdin is not an interactive terminal.

Permission requests contain operation data, not file bodies or environment secrets: write includes path/byte count/precondition state; patch includes paths/hunk counts/dry-run; shell and process include direct argv/cwd/process metadata.

File replacement stages a same-directory temporary file, writes/flushed/syncs it, preserves basic destination permissions, and installs it with an atomic same-filesystem operation. Unix uses rename replacement; Windows uses `ReplaceFileW` for an existing destination and rename only for a new destination. The implementation does not claim preservation of xattrs, ACLs, alternate streams, or other extended metadata.

Multi-file patching is staged and preconditioned, then committed file by file. It is not globally atomic. If a commit partially succeeds, AETHER attempts reverse-order rollback and reports a high-severity `rollback_failed` error with every failed rollback path when rollback is incomplete.

## Bounded resources

Reads, listings, search results, patch inputs, process output, and agent events have explicit limits. Search excludes binary files and honors ignore/hidden policy. Persistent processes are capped at 16 per workspace/session. Each stdout and stderr stream has a 256 KiB recent-output buffer and cumulative `dropped_bytes` metadata; drain tasks continuously consume OS pipes so child output cannot deadlock the process subsystem or grow memory without bound. Worst-case stream storage is 16 × 2 × 256 KiB, excluding small handle/task overhead.

Filesystem work uses one bounded `spawn_blocking` operation per tool call and checks a Tokio-independent `CancellationFlag` between files/chunks/traversal entries and before commit. Atomic replacement is never intentionally interrupted after its OS call begins. Cancellation is returned as the typed `cancelled` category. Established persistent processes survive turn cancellation and are terminated when the interactive session ends; a process cancelled during creation is killed before publication.

## Secrets and network

`RAINY_API_KEY` is read only when a Rainy backend is requested. It is not stored in sessions, tool results, logs, panic diagnostics, or user-visible errors. AETHER has zero telemetry. The only normal outbound operation is an explicit Rainy inference or model-catalog request.

## Dangerous operations

The v0.1 Git contract exposes status, diff, show, log, and branch inspection. Push, remote mutation, hard reset, rebase, filter-branch, and automatic commit are absent. Shell execution is direct argv by default and is permission-gated.
