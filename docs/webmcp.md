# AETHER WebMCP vertical slice

AETHER Fx `v0.1.0-alpha-06` includes a small, public-facing browser surface for the WebMCP
challenge. It is an experimental vertical slice, not a browser IDE and not a replacement for the
native CLI. The page runs a deterministic four-file demo repository entirely in the browser. It
has no backend, account, API key, repository upload, shell, filesystem, network, or arbitrary
JavaScript capability.

## What is shared

The visible page and the WebMCP callbacks use the same `BrowserRuntime` instance and the same
operation boundary:

```text
visible UI ───────┐
                  ├── app.js ──> aether_web.js ──> BrowserRuntime ──> aether-core workflow state
WebMCP callback ──┘                  │
                                     └── bounded in-memory demo files
```

`BrowserRuntime` reuses `aether-core::WorkflowState`, `WorkState`, bounded text, workspace-path
normalization, evidence, mutation, and verification concepts. The native `aether-agent`,
`aether-tools`, Rainy adapter, and optional Obscura process are not compiled into this browser
surface. Browser/WASM cannot honestly provide the native filesystem and process contracts, so the
demo makes that boundary explicit.

The deterministic task is **Reject blank API tokens before gateway handoff**. The initial source
has a length check before normalization. The suggested exact-match patch trims first, rejects a
blank token with `TokenError::Blank`, and preserves the existing short-token guard. The proposed
patch is staged in memory. Only the visible human Accept button changes the shared file. Reject
leaves the file unchanged.

## WebMCP tools

`web/app.js` visibly contains the provider-side registration call:

```js
document.modelContext.registerTool({
  name,
  title,
  description,
  inputSchema,
  annotations,
  execute,
}, { signal: controller.signal });
```

The page registers exactly these six tools when the host exposes
`document.modelContext.registerTool`:

| Tool | Annotation | Behavior |
| --- | --- | --- |
| `inspect_workspace` | `readOnlyHint: true` | Return the bounded file inventory, task, and suggested exact-match patch |
| `read_file` | `readOnlyHint: true` | Read at most 120 numbered lines from one demo file |
| `search_workspace` | `readOnlyHint: true` | Run a bounded case-insensitive literal search over demo files |
| `propose_patch` | `readOnlyHint: false` | Validate and stage one exact-match proposal for human review |
| `verify_patch` | `readOnlyHint: false` | Run bounded browser-safe contract checks against `staged` or `accepted` state |
| `get_task_state` | `readOnlyHint: true` | Return the current task, diff, evidence, workflow, and verification state |

Every input schema sets `additionalProperties: false`. Paths are relative and bounded; patch text,
rationales, queries, results, and file excerpts have explicit byte/count limits. Both the JavaScript
host and the Rust/WASM runtime validate the input, reject path escape and secret-like markers, and
bound the serialized result. `accept_patch` and `reject_patch` are intentionally UI-only operations;
they are not registered as agent tools. A model can prepare and verify a proposal, but it cannot
silently apply one.

The verifier is deliberately honest about its environment. It evaluates the fixed token contract
and checks that the regression fixture is present; it does not pretend to run Cargo, spawn a shell,
or verify an arbitrary uploaded repository. Native release gates remain native Cargo checks.

## Browser compatibility

At startup the page feature-detects `document.modelContext` and `registerTool`. On a compatible
WebMCP host, the header reports the number of registered tools. On an ordinary browser, a yellow
compatibility notice explains that WebMCP is unavailable while all visible controls remain usable.
Registration errors are reported as partial or blocked availability; they never disable the human
workspace or expand the capability set.

The registration API follows the current experimental WebMCP shape documented by the
[Web Machine Learning WebMCP project](https://github.com/webmachinelearning/webmcp). The API and
browser support are experimental and may change.

## Build locally

The checked-in `web/wasm` files are the small distributable browser bundle used by static hosting.
Rebuild them with the pinned Rust toolchain and the matching CLI, then build the static site:

```sh
rustup target add wasm32-unknown-unknown
cargo install --locked --version 0.2.127 wasm-bindgen-cli
npm --prefix web run build:wasm
npm --prefix web run build
```

The final static files are written to `web/dist` (which is ignored). A local smoke server can be
started from that directory, for example:

```sh
python3 -m http.server 4173 --directory web/dist
```

Then open `http://127.0.0.1:4173`. The in-memory demo repository does not read the local checkout,
so the page is safe to run against the AETHER source tree. The Vercel configuration uses
`npm --prefix web run build` and publishes `web/dist`; no build-time or runtime secret is required.

## Prior work and challenge work

Before this extension, AETHER Fx was a native Rust terminal coding agent with bounded local tools,
evidence-gated workflow state, an isolated Rainy adapter, and an optional consent-gated Obscura
browser provider. Those native safeguards remain the product's primary surface. The alpha-06
challenge extension adds only the `aether-web` WASM crate, a static browser UI, provider-side
WebMCP registration, and a deterministic in-memory task so the human/agent workflow can be tried
without granting a web page native authority.

This split is intentional: the browser demo shares the domain/workflow semantics where that is
useful, while native filesystem, process, credentials, model inference, and Obscura lifecycle stay
behind the native boundary. There is no second agent and no browser-to-native bridge.

## Public deployment

The intended deployment is a public, free static Vercel project from this repository. Replace the
placeholder below with the verified production URL before submitting the challenge:

```text
Public demo URL: pending Vercel deployment
```

The deployment must serve `web/dist/index.html`, `web/app.js`, `web/styles.css`, and the adjacent
`web/wasm` bundle over HTTPS. Validate the URL in an ordinary browser and in a WebMCP-enabled host;
the header should show either the connected tool count or the explicit compatibility notice.
