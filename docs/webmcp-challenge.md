# WebMCP challenge submission kit

This file is the handoff material for the AETHER Fx WebMCP challenge entry. The repository and
native CLI are Apache-2.0 licensed. The browser surface is the `alpha-06` extension documented in
[`webmcp.md`](webmcp.md); it is not a separate hosted product.

## Submission fields

### Project title

**AETHER Fx: inspectable coding work with a human gate**

### Live URL

```text
Pending verified Vercel deployment.
```

### Public code

<https://github.com/ferxalbs/aether-fx>

### Description

AETHER Fx is a native Rust coding agent whose core promise is bounded, inspectable work. This
WebMCP extension brings that promise into a deterministic browser workspace: an agent can inspect
a repository-shaped task, read source evidence, search bounded files, prepare an exact-match patch,
and run a browser-safe verification contract. The human still owns the consequential step. The
proposed change remains staged until the visible Accept or Reject gate is used, and the page shows
the same task state, diff, verification, evidence ledger, and tool trace to both sides.

The implementation keeps the boundary small on purpose. A Rust/WASM `BrowserRuntime` reuses
AETHER's bounded workflow and evidence concepts over four in-memory demo files. `app.js` registers
six explicit tools through `document.modelContext.registerTool({ ... })`, validates schemas in the
host and again in Rust, and routes visible controls and agent callbacks through the same runtime.
There is no page-provided shell, filesystem access, secret, network bridge, arbitrary JavaScript
tool, or silent mutation. Unsupported WebMCP hosts receive a compatibility notice and can still
use the visible workflow.

Before this challenge extension, AETHER Fx was already a native terminal agent with workspace
containment, typed permissions, bounded outputs, evidence-gated completion, and an optional
consent-gated Obscura provider. The challenge work is a meaningful new browser/WASM surface built
after that native foundation: static UI, shared state, WebMCP registration, and the human-review
loop. The browser surface does not weaken or replace the native safeguards.

### Why WebMCP fits

Coding work is easier to trust when the agent's available operations are visible and structured.
WebMCP lets the browser host expose narrowly scoped operations while the page keeps the human in
the loop. AETHER uses that model for an inspect → propose → verify → accept/reject flow instead of
turning a webpage into an autonomous IDE. The tool trace and evidence ledger make the shared state
legible during the interaction.

### What was difficult

The difficult part was deciding what not to expose. Native AETHER can use a real canonical
workspace and direct processes; a browser/WASM page cannot honestly provide those capabilities. The
vertical slice therefore uses a bounded fixture, exact-match preconditions, separate staged and
accepted views, content hashes, explicit human evidence, strict schemas, and a compatibility path.
The result demonstrates the same safety shape without claiming to be a native bridge.

## Three-minute demo script

The recording should be under three minutes, use a public/free test URL, and show the URL and the
repository on screen. Use a WebMCP-enabled browser/ChatGPT host for the agent calls; if the host is
not available, the visible controls still demonstrate the same state machine.

1. **0:00–0:20 — Load the page.** Show the repository file tree, active task, source evidence,
   empty diff, evidence ledger, and WebMCP status. Say that the repository is deterministic and
   stays in the tab.
2. **0:20–0:45 — Inspect.** Ask the browser agent to call `inspect_workspace`. Point out that the
   tool trace and task state update in the visible page. Call `read_file` for `src/auth.rs` and
   `search_workspace` for `blank`.
3. **0:45–1:15 — Propose.** Ask the agent to call `propose_patch` using the suggested exact-match
   fields. Show that the diff appears as **Staged**, the source file is not marked changed, and
   Accept/Reject are the only enabled mutation controls.
4. **1:15–1:40 — Verify the proposal.** Call `verify_patch` with `{"scope":"staged"}`. Show the
   three bounded checks and the verification evidence. Explain that this is a browser-safe contract
   check, not a fake Cargo invocation.
5. **1:40–2:10 — Human decision.** Click **Accept**. Show the changed source, the accepted status,
   the human-acceptance evidence, and the verification result. Optionally click **Verify accepted**
   to demonstrate the accepted-scope path.
6. **2:10–2:35 — Inspect shared state.** Ask the agent to call `get_task_state`. Show that it
   receives the same task status, changed file inventory, evidence ledger, and verification data
   visible on the page.
7. **2:35–2:55 — Safety close.** Point to the compatibility notice behavior and say that the
   browser surface has no secrets, arbitrary shell/JavaScript, native filesystem, or silent apply;
   the native AETHER CLI remains the full local product.

## Submission checklist

- Replace the live-URL placeholder in this file and `docs/webmcp.md` with the verified HTTPS URL.
- Confirm the public repository contains the source, checked-in `web/wasm` bundle, license, and
  build instructions.
- Confirm the page source visibly contains `document.modelContext.registerTool({`.
- Test registration and at least one tool call in a supported WebMCP environment.
- Record and publish the short demo video with audio and no private repository data.
- Submit this description, the public URL, repository URL, and video link before the challenge
  deadline.
