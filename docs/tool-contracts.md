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

## Optional Obscura surface

After `/browser` has installed, started, and MCP-validated the pinned Obscura process, the active
registry contains the nine tools above plus exactly these six fixed definitions:

| Tool | Permission | v1 contract |
| --- | --- | --- |
| `browser.tabs` | BrowserRead | Bounded tab and active-tab metadata |
| `browser.navigate` | BrowserRead | Navigate the active tab to a validated public HTTP(S) URL |
| `browser.snapshot` | BrowserRead | Bounded URL, title, and visible page text; never a full HTML response |
| `browser.find` | BrowserRead | Bounded visible-text matches with bounded context |
| `browser.wait` | BrowserRead | Wait for a bounded interval for a CSS selector |
| `browser.performance_audit` | BrowserRead | Fixed read-only navigation/evaluation returning structured page metrics |

The inactive registry remains exactly nine tools, so browser schemas do not enter ordinary prompts.
The active definitions are compiled in AETHER and do not use provider descriptions or schemas to
construct model context. The adapter maps them to the v0.2.1 provider operations
`browser_tab_list`, `browser_navigate`, `browser_snapshot`, `browser_search`, and
`browser_wait_for`; the performance audit uses `browser_navigate` and a fixed internal
`browser_evaluate` expression. Extra Obscura tools are ignored.

Every browser invocation carries an exclusive `BrowserSession(id)` footprint, which serializes
access to the active tab without serializing unrelated workspace reads. `BrowserRead` is gated by
the first-use session permission. `BrowserAction` is reserved for future sensitive operations and
is not part of this surface. Results are bounded for the live turn and reduced to status-only
summaries before session persistence. See [`obscura-provider.md`](obscura-provider.md) for the
static release manifest, MCP handshake, network policy, and lifecycle contract.

## Hosted WebMCP surface

The hosted browser vertical slice is a separate `aether-web` WASM/static surface. It does not add
tools to the native CLI registry and does not connect to the native workspace, Rainy, or Obscura.
When the host supports `document.modelContext.registerTool`, the page registers six bounded tools:

| Tool | Read-only hint | Contract |
| --- | --- | --- |
| `inspect_workspace` | `true` | Four-file in-memory inventory, active task, and exact suggested patch |
| `read_file` | `true` | One bounded demo-file excerpt with line numbers |
| `search_workspace` | `true` | Case-insensitive literal search with bounded matches |
| `propose_patch` | `false` | One exact-match staged proposal; never applies it |
| `verify_patch` | `false` | Browser-safe contract checks against staged or accepted state |
| `get_task_state` | `true` | Shared task, diff, evidence, workflow, and verification state |

The JavaScript host and Rust/WASM runtime both reject unknown fields, unsafe relative paths,
secret-like patch content, and over-limit values. `accept_patch` and `reject_patch` are visible UI
operations only. The human gate is therefore outside the model-visible tool set. See
[`webmcp.md`](webmcp.md) for the state flow, build, compatibility behavior, and deployment notes.
