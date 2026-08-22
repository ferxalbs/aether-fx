# Security model

## Workspace containment

Filesystem tools accept a canonical workspace root. User paths are rejected when absolute, lexical traversal escapes the root, canonicalization escapes the root, or a symlink/junction resolves outside it. Every tool uses the shared workspace resolver.

## Permissions

The typed permission classes are `ReadOnly`, `WorkspaceWrite`, `ProcessExecute`, `ProcessPersistent`, `GitMutate`, `OutsideWorkspace`, and `NetworkMutation`. Policy decisions live in core/tool code; the terminal only presents the actual structured operation (`program`, `arguments`, `cwd`, or file path).

## Bounded resources

Reads, listings, search results, patch inputs, process output, and agent events have explicit limits. Search excludes binary files and honors ignore/hidden policy. Persistent processes have stable IDs and are not detached from cancellation without an explicit future policy.

## Secrets and network

`RAINY_API_KEY` is read only when a Rainy backend is requested. It is not stored in sessions, tool results, logs, panic diagnostics, or user-visible errors. AETHER has zero telemetry. The only normal outbound operation is an explicit Rainy inference or model-catalog request.

## Dangerous operations

The v0.1 Git contract exposes status, diff, show, log, and branch inspection. Push, remote mutation, hard reset, rebase, filter-branch, and automatic commit are absent. Shell execution is direct argv by default and is permission-gated.
