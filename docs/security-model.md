# Security model

## Workspace containment

Filesystem tools accept a canonical workspace root. User paths are rejected when absolute, lexical traversal escapes the root, canonicalization escapes the root, or a symlink/junction resolves outside it. Every tool uses the shared workspace resolver.

## Permissions

The typed permission classes are `ReadOnly`, `WorkspaceWrite`, `ProcessExecute`, `ProcessPersistent`, `GitMutate`, `OutsideWorkspace`, `NetworkMutation`, `BrowserRead`, and `BrowserAction`. Ordinary read-only calls receive an automatic permit; `BrowserRead` is separately gated on first browser use. Other calls produce a structured request and are resolved as allow once, allow for the current session, or deny. Session grants are keyed by tool name plus permission class; a shell grant cannot authorize `write` or `git`, and a future sensitive browser action cannot receive a session grant.

The agent passes an `ExecutionPermit` into every tool. The tool validates the exact `call_id`, tool name, and permission class before mutation or process execution, so a terminal prompt is not the only authorization boundary. Interactive approval is fail-closed when stdin is not an interactive terminal.

Permission requests contain operation data, not file bodies or environment secrets: write includes path/byte count/precondition state; patch includes paths/hunk counts/dry-run; shell and process include direct argv/cwd/process metadata.

Prepared actions retain typed provenance for auditability: model-generated calls are `Model`, repository indexing is `Repository`, and completed local observations are `ToolOutput`; `User` and `Network` are explicit origins rather than inferred from text. Provenance is never user authorization. Repeating tool output inside a later model argument therefore cannot satisfy the user-authority requirement; mutation admission still requires the exact current inspection, content precondition, and independent `ExecutionPermit`.

Observation freshness is separate from authority. Successful non-read discovery and verification
observations are re-executed unless their dependency freshness is proven; this conservative extra
tool call avoids treating a stale search, listing, finding, or verification result as current.
Exact reads may be reused only after a bounded live hash of the canonical file matches the
retained hash. A known, certain shell mutation may clear stale workspace state only after every
affected path has refreshed successfully; uncertain commands and partial refreshes remain stale.
These decisions never grant permission and do not weaken workspace containment, canonical-path,
symlink/reparse, permit, or expected-hash enforcement.

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

`RAINY_API_KEY` is read only when a Rainy backend is requested. It is not stored in sessions, tool results, logs, panic diagnostics, or user-visible errors. AETHER has zero telemetry. Normal startup, `--version`, `--help`, `sessions`, and the default `doctor` are offline. Rainy inference and model-catalog requests remain explicit network operations.

The explicit `aether update` command is a separate fixed-source network boundary. It requests
public releases from `api.github.com/repos/ferxalbs/aether-fx/releases`, ignores drafts, accepts
valid `v`-prefixed SemVer prereleases and stable releases, and selects the highest SemVer
precedence. It constructs the matching GitHub release asset URLs itself and ignores download URLs
from API JSON; no mirror, endpoint override, authorization header, Rainy key, or GitHub token is
accepted. Reqwest uses Rustls, system certificates, and its system-proxy behavior, requires HTTPS
for redirects, uses a ten-second connect limit, a sixty-second per-request limit, and one bounded
retry for transient transport/status failures.

Release metadata and `SHA256SUMS` are capped at 1 MiB. The direct target binary is capped at 128
MiB, streamed into a same-directory temporary file, hashed with SHA-256, checked against one exact
manifest entry, and executed only for a bounded exact `--version` check. The original executable
is inspected as a regular non-link/non-reparse file and its content identity is rechecked before
commit. The updater refuses read-only destinations, non-regular files, and paths that cannot stage
a sibling file; it never invokes sudo, UAC, a package manager, or an installer. Temporary files are
identifiable, removed after success/failure where possible, and
stale files from the same destination are cleaned only on the next explicit update. A temporary
file lock makes a concurrent updater fail clearly.

Unix replaces the existing executable with a same-filesystem atomic rename while preserving its
basic permissions. Windows validates a second temporary helper copy, launches it hidden, exits the
current process, and lets the helper wait up to thirty seconds for the executable handle to be
released before calling `ReplaceFileW`; the original remains intact if the handoff cannot finish.
The Windows JSON `updated: true` result means that this validated handoff was accepted, not that
the current process has already relaunched. HTTPS plus SHA-256 protects transport integrity and
the published manifest relationship but is not an independent cryptographic signature; release
provenance attestations remain optional for clients.

## Session persistence

Repositories contain user/project data. AETHER Fx persistent application and session state is private OS application-state storage, not workspace storage. The state root is selected by `AETHER_FX_STATE_DIR` when set; otherwise Linux/Unix uses `$XDG_STATE_HOME/aether-fx` or `$HOME/.local/state/aether-fx`, macOS uses `$HOME/Library/Application Support/aether-fx`, and Windows uses `%LOCALAPPDATA%\\aether-fx`. The final layout is `workspaces/<workspace-id>/workspace.json`, `workspaces/<workspace-id>/sessions/<session-id>.jsonl`, and the separate local appointment file described below, where the workspace ID is lowercase BLAKE3 hex over the canonical path's native bytes. Persistence is minimized by construction, not by scanning for secrets. A session file stores:

- session identity, workspace root, and model
- committed turn metadata (`turn_id`, step count, cancelled)
- inspected file paths, content hashes, and line ranges
- modified paths
- safe tool summaries (path/hash/count metadata; shell/process/git persist only `ok`/`failed`)
- git branch availability without status/diff text
- sanitized continuation identity (`previous_response_id` or the test `fake_step` key)
- an optional bounded semantic transcript: user/assistant text, tool name and outcome, diagnostics,
  turn summaries, and appointment confirmation summaries

It does not store unsanitized prompts or assistant text, file excerpt bodies, tool output, command
arguments, shell/process stdout or stderr, environment content, permission request details,
credentials, authorization headers, private keys, or tokens. Transcript cells are sanitized,
bounded to the newest fixed cell/byte budget, and passed through the existing secret heuristic;
secret-like cells are omitted rather than aborting the session write. `payload_contains_secrets` is
defense-in-depth after that minimization and is not complete secret detection.

On Unix, the state root, `workspaces`, each workspace bucket, and `sessions` are created and kept at mode `0700`; `workspace.json`, session JSONL files, and compaction temps are `0600`. Windows has no POSIX modes; the same state objects are created without Unix permission bits.

There are two independent boundaries. Workspace tools remain contained by the canonical workspace root. Session storage is contained by the resolved AETHER state root: AETHER canonicalizes the workspace, derives a path-safe hash bucket, atomically creates or validates `workspace.json`, and creates/opens state directories, metadata, session JSONL, and compaction temps without following symbolic links or Windows reparse points. Existing state directories must be real directories and existing state files must be regular files; a link or reparse point is rejected. Unix uses `openat`/`mkdirat` with `O_NOFOLLOW` (and `O_EXCL` for metadata and temps). Windows inspects `FILE_ATTRIBUTE_REPARSE_POINT` and opens with `FILE_FLAG_OPEN_REPARSE_POINT` so a reparse name cannot redirect the write. Session IDs are validated before becoming file names, and workspace IDs are generated rather than accepted as paths. This is fail-closed containment, not a claim of perfect adversarial TOCTOU immunity: an external process can still mutate the namespace in the window after validation and before the OS create/open call. Session metadata and JSONL are never intentionally written inside a repository.

Schema version 6 also permits a `Checkpoint` record for a cancelled, step-limited, or otherwise
incomplete turn and carries the optional bounded transcript snapshot. Versions 3 through 5 remain
readable; older records simply restore agent context without transcript cells and the TUI shows a
clear transcript-unavailable marker. A checkpoint is resumable state, never a completion claim; it
contains the same minimized context and sanitized continuation as a normal turn. Each completed
turn is one JSONL record. A truncated final record is an uncommitted crash window and is ignored;
the previous committed turn or checkpoint remains valid. Corrupt middle records fail replay.
Malformed individual transcript cells are skipped so recoverable session context remains usable.
Compaction retains the bounded transcript from retained snapshots and never overwrites a file after
a failed replay. The persisted work objective is normalized to a bounded first-line summary and
obvious secret assignments are redacted; raw prompts and reasoning are not stored outside the
safe semantic transcript fields.

Resume re-hashes inspected files before resumed actions and again before completion. If the live
workspace root differs, or an inspected file is stale or missing, Rainy continuation is discarded,
the structured work evidence is invalidated, and the next model turn reconstructs from bounded
local context. Stale remote continuation never outranks the filesystem. Dirty paths detected from
Git remain user-owned and are not silently overwritten; an attempted mutation still requires
current inspection, a matching expected hash, and an independent permit.

Compiler/test recovery and post-mutation context refreshes are observations, not authority. They
read at most the bounded source window after rejecting symlinks, non-files, and canonical paths
outside the workspace. A cached read is reused only for a complete retained range whose content
hash still matches a bounded live re-hash; otherwise the normal read tool executes. Mutation
admission continues to require its own typed permit, containment checks, and expected-hash
revalidation. Installing a verification plan normalizes each step to `pending`; completion
requires an observed successful verification for the current workspace revision, so a model's
premature finish claim cannot establish completion. The deterministic negative completion eval
asserts that such a claim is rejected. The extra re-execution under uncertain freshness is an
intentional bounded cost for fail-closed evidence handling.

## Interactive, headless, and terminal-integrity boundaries

Raw terminal mode is entered only after both standard streams have been identified as terminals.
The `aether-tui` RAII session restores raw mode, cursor visibility, bracketed paste, line wrapping,
and the inline/alternate-screen boundary on normal return, cancellation, error, or unwind. Its
approval overlay resolves the existing permission broker; the overlay itself never authorizes a
tool, bypasses expected-hash checks, or changes the typed permission policy. Pipe, JSON, one-shot,
and `--non-interactive` invocations use bounded plain-line input, no prompt/banner/ANSI output,
and a `NoPermissionBroker` unless the user explicitly selected the existing `--yolo` authority
mode. They do not initialize the TUI or mutate appointment state.

Catalog, session, and model strings are untrusted display data. They are bounded, sanitized for
control characters, truncated by terminal display cells without splitting graphemes, and ordered
deterministically before rendering. Catalog fields that are absent remain `unknown`; AETHER does
not infer capabilities, access, context, tier, or reasoning support from a model name. Raw catalog
JSON is not sent through the renderer.

## Local appointment state

`/schedule` exists only in the interactive TUI. It collects a title, strict local date/time,
explicit UTC offset, duration, and bounded optional metadata; a valid draft is normalized with
`time::OffsetDateTime` to a canonical UTC RFC 3339 instant and shown in a review card. The user
must explicitly confirm that card before the binary calls `LocalCalendarAdapter`. No natural
language turn, model-visible tool, Rainy request, browser action, credential lookup, network
calendar provider, recurrence, reminder, conflict check, or remote identifier is involved.

Appointments are stored outside the workspace at the private state root under
`workspaces/<workspace-id>/appointments.json`. The file has a versioned top-level record and a
bounded appointment array; the adapter rejects malformed, oversized, indirect, symlinked, or
reparse-point state and refuses paths outside the validated AETHER state root. New state is
serialized with bounded text and attendee limits, written to a same-directory temporary file,
flushed and synced, and installed with an atomic replacement. Existing state remains untouched if
validation, serialization, or commit fails. Unix directories/files use the same restrictive
user-only permissions as session storage. Appointment IDs are generated locally from the
canonical appointment payload and creation timestamp with BLAKE3; provider IDs are never
synthesized. Appointment confirmation cells may be copied into a session's safe transcript, but
the appointment file is not exposed as model context or tool output.

## Configuration and authentication

Non-secret preferences live in `config.json` at the private application-state root, never in a
repository. AETHER creates state directories with user-only `0700` permissions and configuration
files with `0600` permissions on Unix, writes through a same-directory temporary file, and rejects
symbolic links/reparse points at the state and configuration boundaries. The configuration schema
is deliberately small: a bounded default model and an optional bounded direct argv credential
helper. `config show` exposes the default model and a helper configured/not-configured boolean,
never helper arguments or secret output.

Credential precedence is environment first, then the configured helper. A helper is started with
direct `Command` argv semantics, no shell, null stdin/stderr, a 16 KiB stdout ceiling, and a
five-second deadline. Its output is accepted only as a bounded UTF-8 key for the current process;
it is not written to config, sessions, logs, JSON output, or model context. Interactive secret
input has no echo or masking bytes, accepts Ctrl-C/Escape as cancellation, and restores the
terminal before returning. There is intentionally no `--key` option because secrets in argv can
leak through process listings, shell history, CI diagnostics, and crash reports.

The default doctor is offline and reports only presence/configuration facts. Network catalog
diagnostics require an explicit `--network`; neither path prints authorization headers, raw keys,
helper output, or other credential material.

## Dangerous operations

The v0.1 Git contract exposes status, diff, show, log, and branch inspection. Push, remote mutation, hard reset, rebase, filter-branch, and automatic commit are absent. Shell execution is direct argv by default and is permission-gated.

## External Obscura provider

Obscura is a separately distributed process, not a library linked into the AETHER browser or
workspace path. The adapter accepts only the exact v0.2.1 artifact selected by the compiled target
manifest. It shows the release metadata and requires explicit interactive consent before any
download; `--yolo` is deliberately not part of that decision. The installer follows only HTTPS
redirects from the fixed GitHub asset URL, enforces the embedded size and SHA-256, extracts exactly
the expected binary and worker, rejects symlinks/reparse points and unsafe archive members,
validates the exact `obscura --version` output, and activates the version directory atomically.
No package manager, sudo, UAC, environment-provided replacement binary, or remote `latest` lookup
is used. A download or validation failure does not replace a previous installation. The upstream
Obscura v0.2.1 project and release binary are Apache-2.0; direct adapter dependency notices are
listed in [`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md).

The process is spawned from an absolute validated path without a shell, with a private working
directory, cleared inherited environment, and piped stdio. `RAINY_API_KEY`, proxy credentials, and
unrelated host variables are not inherited. One MCP channel is serialized per AETHER session;
frames, results, stderr, and request duration are capped. The handshake requires MCP
`2024-11-05`, `notifications/initialized`, and a compatible `tools/list`. Extra tools are ignored,
provider descriptions are never used as model instructions, and `tools/list_changed` or any
unsupported server message fails closed. Timeout/cancellation and process failure invalidate the
supervisor, so a crash cannot trigger a silent restart or download. Shutdown closes stdin, waits a
bounded interval, then terminates the parent as a fallback.

## Browser network and page-content boundary

AETHER rejects `file:`, `data:`, `javascript:`, `chrome:`, and `devtools:` URLs, embedded
credentials, and literal private, metadata, unique-local, link-local, multicast, unspecified,
documentation, and reserved addresses. The provider's resolver and redirect checks must reject a
public hostname that resolves or redirects to a forbidden destination; URL parsing in AETHER
cannot by itself prove the effective destination. AETHER never passes Obscura's broad
`--allow-private-network` option or its environment equivalent.

The published v0.2.1 provider blocks loopback together with private addresses when its safe default
is active. Because that release has no loopback-only option, AETHER rejects `localhost`,
`127.0.0.0/8`, and `::1` rather than weakening the policy. The effective v1 surface is therefore
public HTTP/HTTPS only until a future Obscura release publishes a narrow loopback contract. No
existing Chrome profile is opened, and no workspace file is exposed through `file:`.

The model-visible allowlist is exactly six read-oriented operations. `browser.snapshot` is bounded
visible text, `browser.find` is bounded search, and `browser.performance_audit` invokes only an
adapter-owned fixed expression. Click, type, submit, arbitrary JavaScript, downloads, screenshots,
PDFs, and generic MCP passthrough are not registered. Each call requires `BrowserRead` on first
use and carries an exclusive `BrowserSession(id)` footprint. `BrowserAction` exists as a reserved
class only; it is disabled in v1 and cannot be converted into an `AllowSession` grant.

External research is a distinct agent mode. It exposes browser tools and local read-only
inspection, rejects mutating or process tools even when a model invents their names, and completes
without repository evidence requirements. Page content is data, not user authorization or system
instructions. Durable session state keeps only status summaries such as `browser.snapshot ok`;
cookies/storage state is private provider state, while page bodies, HTML, authorization headers,
complete MCP payloads, request/process IDs, and page instructions are not persisted.

The native Obscura integration does not dynamically import page tools. Separately, alpha-07 ships a
public `aether-web` WASM demo whose own page registers six explicit WebMCP tools when the host
supports `document.modelContext.registerTool`. That page uses only a deterministic in-memory
fixture: it cannot access the native checkout, invoke a process, contact Rainy/Obscura, read a
secret, or grant itself permissions. `inspect_workspace`, `read_file`, `search_workspace`, and
`get_task_state` are read-only; `propose_patch` only stages; `verify_patch` records bounded
browser-safe evidence; and `accept_patch`/`reject_patch` remain visible UI-only operations.

The page validates every tool input in JavaScript and again in Rust, rejects unknown fields and
workspace escapes, caps requests/results/files/lines, rejects secret-like patch markers, and marks
page content as untrusted in the tool annotations. It feature-detects the API and shows a
compatibility notice when registration is unavailable or partial. The browser surface is not a
native workspace bridge and does not weaken any native permission, containment, hash, session, or
provenance control. See [`webmcp.md`](webmcp.md) for the complete boundary.
