# Security model

## Workspace containment

Filesystem tools accept a canonical workspace root. User paths are rejected when absolute, lexical traversal escapes the root, canonicalization escapes the root, or a symlink/junction resolves outside it. Every tool uses the shared workspace resolver.

## Permissions

The typed permission classes are `ReadOnly`, `WorkspaceWrite`, `ProcessExecute`, `ProcessPersistent`, `GitMutate`, `OutsideWorkspace`, and `NetworkMutation`. Read-only calls receive an automatic permit. Other calls produce a structured request and are resolved as allow once, allow for the current session, or deny. Session grants are keyed by tool name plus permission class; a shell grant cannot authorize `write` or `git`.

The agent passes an `ExecutionPermit` into every tool. The tool validates the exact `call_id`, tool name, and permission class before mutation or process execution, so a terminal prompt is not the only authorization boundary. Interactive approval is fail-closed when stdin is not an interactive terminal.

Permission requests contain operation data, not file bodies or environment secrets: write includes path/byte count/precondition state; patch includes paths/hunk counts/dry-run; shell and process include direct argv/cwd/process metadata.

File replacement stages a same-directory temporary file, writes/flushed/syncs it, preserves basic destination permissions, and installs it with an atomic same-filesystem operation. For an existing destination, Unix uses rename replacement and Windows uses `ReplaceFileW`; this is atomic replacement, not create-if-absent. For `create_only=true` (and for a destination that was missing when an ordinary write began), the commit uses a native no-replace install: Linux/macOS `renameat_with(NOREPLACE)` / `RENAME_EXCL`, Windows `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`, and a hard-link fallback only when exclusive rename is unsupported on the filesystem. The destination is never replaced by that path. Filesystems that provide neither exclusive rename nor hard links cannot atomically create-if-absent; AETHER fails closed rather than copying over an existing file. The implementation does not claim preservation of xattrs, ACLs, alternate streams, or other extended metadata.

`write` and `patch` share a weak-reference per-destination mutation coordinator inside a workspace. This serializes AETHER mutations to the same resolved destination without imposing one global filesystem lock. Multi-file patches normalize, sort, and deduplicate keys before acquiring locks, preventing lock-order deadlocks. A patch stages all replacements before committing and revalidates each source state immediately before its commit.

`expected_hash` is an optimistic content precondition: AETHER hashes the target while holding its destination guard and hashes it again before commit, rejecting a changed hash as `concurrent_modification`. Existing-destination writes also retain and revalidate that content hash even without a caller-supplied precondition; missing-destination writes revalidate bounded filesystem metadata (existence, length, modification time, and symlink state). These checks are not a fully linearizable cross-process compare-and-swap: an external actor can still mutate the namespace after revalidation and before the final atomic OS call. The final replacement/create operation remains atomic, and AETHER-vs-AETHER same-path preparation/commit sections are serialized.

Multi-file patching is staged and preconditioned, then committed file by file. It is not globally atomic. If a commit partially succeeds, AETHER attempts reverse-order rollback and reports a high-severity `rollback_failed` error with every failed rollback path when rollback is incomplete.

## Bounded resources

Reads, listings, search results, patch inputs, process output, and agent events have explicit limits. Search excludes binary files and honors ignore/hidden policy. Persistent processes are capped at 16 per workspace/session. Each stdout and stderr stream has a 256 KiB recent-output buffer and cumulative `dropped_bytes` metadata; drain tasks continuously consume OS pipes so child output cannot deadlock the process subsystem or grow memory without bound. Drain task handles remain owned by the process handle and are joined or aborted with a one-second bound after confirmed child termination. Worst-case stream storage is 16 × 2 × 256 KiB, excluding small handle/task overhead.

Filesystem work uses one bounded `spawn_blocking` operation per tool call and checks a Tokio-independent `CancellationFlag` between files/chunks/traversal entries and before commit. Atomic replacement is never intentionally interrupted after its OS call begins. Cancellation is returned as the typed `cancelled` category. Established persistent processes survive turn cancellation and are terminated when the interactive session ends; a process cancelled during creation is killed before publication.

Process termination first checks for an already-exited child, otherwise requests termination and waits up to five seconds for a known terminal state. The registry entry is removed only after termination is confirmed; kill errors, wait errors, and timeouts leave the process tracked and surface as `termination_failed`. Session shutdown attempts every owned process and returns an aggregate report rather than stopping at the first failure. Interactive shutdown prints a concise high-severity diagnostic when any child may remain alive.

Ctrl+C in a permission prompt is a terminal `CancelTurn` outcome: it cancels the turn and removes the pending broker request. Typing `n` remains the ordinary `deny` decision, which is returned to the model as `permission_denied` so the turn may continue.

## Secrets and network

`RAINY_API_KEY` is read only when a Rainy backend is requested. It is not stored in sessions, tool results, logs, panic diagnostics, or user-visible errors. AETHER has zero telemetry. The only normal outbound operation is an explicit Rainy inference or model-catalog request.

## Session persistence

Local sessions are JSONL under `<workspace>/.aether/sessions/<session-id>.jsonl` (schema version 3). Persistence is minimized by construction, not by scanning for secrets. A session file stores:

- session identity, workspace root, and model
- committed turn metadata (`turn_id`, step count, cancelled)
- inspected file paths, content hashes, and line ranges
- modified paths
- safe tool summaries (path/hash/count metadata; shell/process/git persist only `ok`/`failed`)
- git branch availability without status/diff text
- sanitized continuation identity (`previous_response_id` or the test `fake_step` key)

It does not store raw prompts, assistant text, file excerpt bodies, shell/process stdout or stderr, environment content, credentials, authorization headers, private keys, or tokens. `payload_contains_secrets` is defense-in-depth after that minimization and is not complete secret detection.

On Unix, `.aether/` and `.aether/sessions/` are created and kept at mode `0700`; session JSONL files and compaction temps are `0600`. Windows has no POSIX modes; the same paths are created without Unix permission bits.

Session storage is workspace-contained. AETHER canonicalizes the workspace, then creates and opens `.aether`, `.aether/sessions`, session JSONL, and compaction temps without following symbolic links or Windows reparse points. Existing session directories must be real directories and existing session files must be regular files; a link or reparse point is rejected. Unix uses `openat`/`mkdirat` with `O_NOFOLLOW` (and `O_EXCL` for temps). Windows inspects `FILE_ATTRIBUTE_REPARSE_POINT` and opens with `FILE_FLAG_OPEN_REPARSE_POINT` so a reparse name cannot redirect the write. This is fail-closed containment, not a claim of perfect adversarial TOCTOU immunity: an external process can still mutate the namespace in the window after validation and before the OS create/open call. Session metadata is never intentionally written outside the canonical workspace.

Each completed turn is one JSONL record. A truncated final record is an uncommitted crash window and is ignored; the previous committed turn remains valid. Corrupt middle records fail replay. Compaction never overwrites a file after a failed replay.

Resume re-hashes inspected files. If the live workspace root differs, or an inspected file is stale or missing, Rainy continuation is discarded and the next model turn reconstructs from bounded local context. Stale remote continuation never outranks the filesystem.

## Dangerous operations

The v0.1 Git contract exposes status, diff, show, log, and branch inspection. Push, remote mutation, hard reset, rebase, filter-branch, and automatic commit are absent. Shell execution is direct argv by default and is permission-gated.
